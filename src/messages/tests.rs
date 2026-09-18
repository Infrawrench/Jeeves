use super::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
use twilight_model::gateway::payload::incoming::{
    MessageCreate, MessageDelete, MessageDeleteBulk, MessageUpdate,
};

fn message(id: i64, guild: i64, channel: i64, image: Option<i64>) -> Message {
    let attachments = image
        .map(|id| {
            vec![json!({
                "id": id.to_string(), "filename": "photo.png", "size": 64,
                "url": format!("https://cdn.discordapp.com/attachments/{id}.png"),
                "proxy_url": format!("https://media.discordapp.net/attachments/{id}.png"),
                "content_type": "image/png"
            })]
        })
        .unwrap_or_default();
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild.to_string(), "channel_id": channel.to_string(),
        "author": {"id": "42", "username": "test", "discriminator": "0", "avatar": null},
        "content": "a test message", "timestamp": "2035-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": attachments, "embeds": [], "pinned": false, "type": 0
    })).unwrap()
}

fn captions(message: &Message) -> Vec<ImageResult> {
    message
        .attachments
        .iter()
        .map(|attachment| ImageResult {
            attachment_id: attachment.id.get() as i64,
            url: attachment.url.clone(),
            mime_type: "image/png".into(),
            description: Some("A red bicycle.".into()),
            description_error: None,
        })
        .collect()
}

async fn mock_describe(_: String, _: String) -> Result<String> {
    Ok("A red bicycle.".into())
}

async fn noop_handler(_: MessageContext) -> Result<()> {
    Ok(())
}

async fn test_pool(size: u32) -> Result<PgPool> {
    crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        size,
    )
    .await
}

fn test_base(prefix: i64) -> i64 {
    prefix + i64::from(std::process::id()) * 100_000
}

#[test]
fn supported_image_attachments_are_identified() {
    let mut attachment = message(1, 2, 3, Some(4)).attachments.remove(0);
    assert_eq!(image_mime(&attachment), Some("image/png"));
    attachment.content_type = None;
    attachment.filename = "PHOTO.JPEG".into();
    assert_eq!(image_mime(&attachment), Some("image/jpeg"));
    attachment.content_type = Some("application/pdf".into());
    assert_eq!(image_mime(&attachment), None);
}

#[tokio::test]
async fn image_results_preserve_failures_and_reuse_successful_captions() -> Result<()> {
    let mut incoming = message(1, 2, 3, Some(101));
    let cached = captions(&incoming);
    for id in 102..=105 {
        incoming
            .attachments
            .push(message(1, 2, 3, Some(id)).attachments.remove(0));
    }
    incoming.attachments[0].url.push_str("?refreshed=true");
    incoming.attachments[1].size = MAX_IMAGE_BYTES as u64 + 1;
    incoming.attachments[2].content_type = Some("application/pdf".into());
    let calls = Arc::new(AtomicUsize::new(0));
    let images = transcribe_images(&incoming, &cached, {
        let calls = calls.clone();
        move |url: String, mime: String| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(mime, "image/png");
                if url.ends_with("/104.png") {
                    anyhow::bail!("mock Gemini failure")
                }
                Ok("Fresh image text.".into())
            }
        }
    })
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        images
            .iter()
            .map(|image| image.attachment_id)
            .collect::<Vec<_>>(),
        [101, 102, 104, 105]
    );
    assert_eq!(images[0].description.as_deref(), Some("A red bicycle."));
    assert!(images[0].url.ends_with("?refreshed=true"));
    assert!(images[1].description.is_none());
    assert!(
        images[1]
            .description_error
            .as_deref()
            .unwrap()
            .contains("12 MiB")
    );
    assert_eq!(
        images[2].description_error.as_deref(),
        Some("mock Gemini failure")
    );
    assert_eq!(images[3].description.as_deref(), Some("Fresh image text."));
    let empty = transcribe_images(&message(1, 2, 3, None), &[], mock_describe).await?;
    assert!(empty.is_empty());
    Ok(())
}

#[tokio::test]
async fn image_transcription_is_parallel_and_bounded() -> Result<()> {
    let mut incoming = message(1, 2, 3, None);
    for id in 1..=5 {
        incoming
            .attachments
            .push(message(1, 2, 3, Some(id)).attachments.remove(0));
    }
    let gate = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let processor = tokio::spawn({
        let gate = gate.clone();
        async move {
            transcribe_images(&incoming, &[], move |url: String, _: String| {
                let gate = gate.clone();
                let started_tx = started_tx.clone();
                async move {
                    started_tx.send(url)?;
                    gate.acquire_owned().await?.forget();
                    Ok("caption".into())
                }
            })
            .await
        }
    });
    for _ in 0..MAX_IMAGE_REQUESTS {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await?
                .is_some()
        );
    }
    assert!(started_rx.try_recv().is_err());
    assert!(!processor.is_finished());
    gate.add_permits(MAX_IMAGE_REQUESTS);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
            .await?
            .is_some()
    );
    gate.add_permits(1);
    let images = tokio::time::timeout(Duration::from_secs(5), processor).await???;
    assert_eq!(
        images
            .iter()
            .map(|image| image.attachment_id)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn message_storage_smoke() -> Result<()> {
    let pool = test_pool(8).await?;
    let base = test_base(9_000_000_000_000_000);
    let guild = base;
    let channel = base + 1;
    let removed: bool = sqlx::query_scalar("SELECT to_regclass('public.message_images') IS NULL")
        .fetch_one(&pool)
        .await?;
    assert!(removed);
    let mut first = message(base + 10, guild, channel, Some(base + 90_000));
    first.timestamp = "2026-01-01T00:00:00.000000+00:00".parse()?;
    save(&pool, &first, &captions(&first), false).await?;
    sqlx::query(
        "INSERT INTO messages (id, guild_id, channel_id, author_id, content, timestamp)
         SELECT $1 + n, $2, $3, 42, 'retention test', '2026-01-01'::timestamptz + n * interval '1 second'
         FROM generate_series(11, 510) n",
    ).bind(base).bind(guild).bind(channel).execute(&pool).await?;
    let (count, oldest): (i64, i64) =
        sqlx::query_as("SELECT count(*), min(id) FROM messages WHERE channel_id = $1")
            .bind(channel)
            .fetch_one(&pool)
            .await?;
    assert_eq!((count, oldest), (500, base + 11));
    assert!(cached_images(&pool, &first).await?.is_none());

    first.id = Id::new((base + 999) as u64);
    save(&pool, &first, &captions(&first), false).await?;
    assert!(!exists(&pool, base + 999).await?);
    let other = message(base + 1_000, guild, channel + 1, None);
    save(&pool, &other, &[], false).await?;
    let mut tasks = JoinSet::new();
    for n in 0..32_i64 {
        let pool = pool.clone();
        tasks.spawn(async move {
            sqlx::query(
                "INSERT INTO messages (id, guild_id, channel_id, author_id, content, timestamp)
                 VALUES ($1, $2, $3, 42, 'concurrent', '2030-01-01'::timestamptz)",
            )
            .bind(base + 2_000 + n)
            .bind(guild)
            .bind(channel)
            .execute(&pool)
            .await
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE channel_id = $1")
        .bind(channel)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 500);
    assert!(exists(&pool, base + 1000).await?);
    assert!(!exists(&pool, base + 11).await?);
    assert!(exists(&pool, base + 43).await?);

    let photo = message(base + 3000, guild, channel, Some(base + 90_001));
    save(&pool, &photo, &captions(&photo), false).await?;
    assert_eq!(
        cached_images(&pool, &photo).await?.unwrap(),
        captions(&photo)
    );
    let mut edited = photo.clone();
    edited.attachments.clear();
    edited.content = "removed the photo".into();
    save(&pool, &edited, &[], true).await?;
    assert!(cached_images(&pool, &edited).await?.unwrap().is_empty());
    delete(&pool, channel, &[base + 3000]).await?;
    save(&pool, &photo, &captions(&photo), true).await?;
    assert!(!exists(&pool, base + 3000).await?);

    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(ingest_with_handler(
        pool.clone(),
        rx,
        mock_describe,
        noop_handler,
    ));
    let ordered = message(base + 3002, guild, channel, Some(base + 90_003));
    tx.send(Event::MessageCreate(Box::new(MessageCreate(
        ordered.clone(),
    ))))
    .await?;
    tx.send(Event::MessageDelete(MessageDelete {
        id: ordered.id,
        channel_id: ordered.channel_id,
        guild_id: ordered.guild_id,
    }))
    .await?;
    tx.send(Event::MessageDeleteBulk(MessageDeleteBulk {
        ids: vec![other.id],
        channel_id: ordered.channel_id,
        guild_id: ordered.guild_id,
    }))
    .await?;
    drop(tx);
    worker.await??;
    assert!(!exists(&pool, base + 3002).await?);
    assert!(exists(&pool, base + 1000).await?);
    cleanup(&pool, &[guild]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn message_action_context_smoke() -> Result<()> {
    let pool = test_pool(1).await?;
    let base = test_base(9_200_000_000_000_000);
    let guild = base;
    let channel = base + 10;
    let first = message(base + 100, guild, channel, Some(base + 1000));
    let second = message(base + 101, guild, channel, None);
    for message in [
        &first,
        &second,
        &message(base + 102, guild, channel + 1, None),
        &message(base + 103, guild + 1, channel, None),
    ] {
        save(&pool, message, &captions(message), false).await?;
    }
    let global = add_action(&pool, guild, None, "Is this spam?").await?;
    let selected = add_action(
        &pool,
        guild,
        Some(vec![channel, channel + 1]),
        "Is this relevant?",
    )
    .await?;
    add_action(&pool, guild, Some(vec![channel + 1]), "Different channel").await?;
    add_action(&pool, guild, Some(vec![]), "No channels").await?;
    add_action(&pool, guild + 1, None, "Different guild").await?;
    let incoming = message(base + 104, guild, channel, Some(base + 1004));
    let context = tokio::time::timeout(
        Duration::from_secs(5),
        message_actions::load_context(&pool, &incoming, captions(&incoming)),
    )
    .await??
    .context("expected guild context")?;
    assert_eq!(
        context
            .history
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        [base + 100, base + 101]
    );
    assert_eq!(context.history[0].images, captions(&first));
    assert!(context.history[1].images.is_empty());
    assert_eq!(context.images, captions(&incoming));
    assert_eq!(
        context
            .actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<_>>(),
        [global, selected]
    );
    assert_eq!(context.actions[0].question, "Is this spam?");
    assert_eq!(context.actions[0].code.as_deref(), Some("return true;"));
    assert!(context.actions[0].only_channels.is_none());
    assert_eq!(
        context.actions[1].only_channels,
        Some(vec![channel, channel + 1])
    );
    assert!(!exists(&pool, base + 104).await?);
    let repeated = message_actions::load_context(&pool, &first, captions(&first))
        .await?
        .unwrap();
    assert_eq!(repeated.history.len(), 1);
    assert_eq!(repeated.history[0].id, base + 101);
    let empty = message_actions::load_context(
        &pool,
        &message(base + 105, guild + 2, channel + 2, None),
        vec![],
    )
    .await?
    .unwrap();
    assert!(empty.history.is_empty() && empty.actions.is_empty());
    let mut dm = incoming;
    dm.guild_id = None;
    assert!(
        message_actions::load_context(&pool, &dm, vec![])
            .await?
            .is_none()
    );
    cleanup(&pool, &[guild, guild + 1]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn message_action_dispatch_smoke() -> Result<()> {
    let pool = test_pool(4).await?;
    let base = test_base(9_300_000_000_000_000);
    let guild = base;
    let channel = base + 10;
    sqlx::query(
        "INSERT INTO messages (id, guild_id, channel_id, author_id, content, timestamp)
         SELECT $1 + n, $2, $3, 42, 'history', '2026-01-01'::timestamptz + n * interval '1 second'
         FROM generate_series(1000, 1499) n",
    )
    .bind(base)
    .bind(guild)
    .bind(channel)
    .execute(&pool)
    .await?;
    let action = add_action(&pool, guild, None, "Should this be reviewed?").await?;
    let mut incoming = message(base + 2000, guild, channel, Some(base + 3000));
    incoming.attachments.push(
        message(1, guild, channel, Some(base + 3001))
            .attachments
            .remove(0),
    );
    let image_gate = Arc::new(Semaphore::new(0));
    let action_gate = Arc::new(Semaphore::new(0));
    let (image_started_tx, mut image_started_rx) = mpsc::unbounded_channel();
    let (actions_tx, mut actions_rx) = mpsc::unbounded_channel();
    let (reports_tx, mut reports_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(8);
    let describe = {
        let gate = image_gate.clone();
        move |url: String, _: String| {
            let gate = gate.clone();
            let started = image_started_tx.clone();
            async move {
                started.send(())?;
                gate.acquire_owned().await?.forget();
                if url.ends_with(&format!("/{}.png", base + 3001)) {
                    anyhow::bail!("mock Gemini failure")
                }
                Ok("The image says hello.".into())
            }
        }
    };
    let handler = {
        let gate = action_gate.clone();
        move |context: MessageContext| {
            let gate = gate.clone();
            let actions_tx = actions_tx.clone();
            let reports_tx = reports_tx.clone();
            async move {
                let report = message_actions::process_message_with_handler(
                    context,
                    move |context, action| {
                        let gate = gate.clone();
                        let actions_tx = actions_tx.clone();
                        async move {
                            actions_tx.send((action.id, context))?;
                            gate.acquire_owned().await?.forget();
                            Ok(action.id)
                        }
                    },
                )
                .await;
                reports_tx.send(report)?;
                Ok(())
            }
        }
    };
    let worker = tokio::spawn(ingest_with_handler(
        pool.clone(),
        events_rx,
        describe,
        handler,
    ));
    events_tx
        .send(Event::MessageCreate(Box::new(MessageCreate(
            incoming.clone(),
        ))))
        .await?;
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), image_started_rx.recv())
            .await?
            .unwrap();
    }
    assert!(!exists(&pool, base + 2000).await?);
    assert!(actions_rx.try_recv().is_err());
    // This action must be visible: context queries happen AFTER image preparation.
    let late_action = add_action(&pool, guild, None, "Added while images were running").await?;
    image_gate.add_permits(2);
    let mut seen_actions = Vec::new();
    let mut snapshots = Vec::new();
    for _ in 0..2 {
        let (id, snapshot) = tokio::time::timeout(Duration::from_secs(5), actions_rx.recv())
            .await?
            .unwrap();
        seen_actions.push(id);
        snapshots.push(snapshot);
    }
    seen_actions.sort_unstable();
    assert_eq!(seen_actions, [action, late_action]);
    assert!(Arc::ptr_eq(&snapshots[0], &snapshots[1]));
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.history.len(), 500);
    assert_eq!(snapshot.history.first().unwrap().id, base + 1000);
    assert_eq!(snapshot.history.last().unwrap().id, base + 1499);
    assert_eq!(
        snapshot.images[0].description.as_deref(),
        Some("The image says hello.")
    );
    assert_eq!(
        snapshot.images[1].description_error.as_deref(),
        Some("mock Gemini failure")
    );

    // Storage completes while both per-action handlers are still blocked.
    wait_until_stored(&pool, base + 2000).await?;
    assert_eq!(
        cached_images(&pool, &incoming).await?.unwrap(),
        snapshot.images
    );
    assert!(!exists(&pool, base + 1000).await?);
    assert!(reports_rx.try_recv().is_err());
    drop(events_tx);
    assert!(!worker.is_finished());
    action_gate.add_permits(2);
    tokio::time::timeout(Duration::from_secs(5), worker).await???;
    let report = reports_rx.recv().await.unwrap();
    assert_eq!((report.succeeded(), report.failed()), (2, 0));
    cleanup(&pool, &[guild]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn message_image_edit_and_delete_smoke() -> Result<()> {
    let pool = test_pool(4).await?;
    let base = test_base(9_400_000_000_000_000);
    let guild = base;
    let channel = base + 10;
    let original = message(base + 100, guild, channel, Some(base + 1000));
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (contexts_tx, mut contexts_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(16);
    let worker = tokio::spawn(ingest_with_handler(
        pool.clone(),
        events_rx,
        {
            let calls = calls.clone();
            let gate = gate.clone();
            move |_: String, _: String| {
                let calls = calls.clone();
                let gate = gate.clone();
                let started_tx = started_tx.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(())?;
                    gate.acquire_owned().await?.forget();
                    Ok("cached caption".into())
                }
            }
        },
        move |context| {
            let contexts_tx = contexts_tx.clone();
            async move {
                contexts_tx.send(context)?;
                Ok(())
            }
        },
    ));
    events_tx
        .send(Event::MessageCreate(Box::new(MessageCreate(
            original.clone(),
        ))))
        .await?;
    tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
        .await?
        .unwrap();
    let mut edited = original.clone();
    edited.content = "text-only edit".into();
    events_tx
        .send(Event::MessageUpdate(Box::new(MessageUpdate(
            edited.clone(),
        ))))
        .await?;
    edited.attachments.push(
        message(1, guild, channel, Some(base + 1001))
            .attachments
            .remove(0),
    );
    events_tx
        .send(Event::MessageUpdate(Box::new(MessageUpdate(
            edited.clone(),
        ))))
        .await?;
    edited.attachments.remove(0);
    edited.content = "removed first image".into();
    events_tx
        .send(Event::MessageUpdate(Box::new(MessageUpdate(
            edited.clone(),
        ))))
        .await?;
    // Delete is queued while the original description has not yet completed.
    events_tx
        .send(Event::MessageDelete(MessageDelete {
            id: original.id,
            guild_id: original.guild_id,
            channel_id: original.channel_id,
        }))
        .await?;
    events_tx
        .send(Event::MessageUpdate(Box::new(MessageUpdate(
            original.clone(),
        ))))
        .await?;
    let mut dm = original.clone();
    dm.id = Id::new((base + 101) as u64);
    dm.guild_id = None;
    events_tx
        .send(Event::MessageCreate(Box::new(MessageCreate(dm))))
        .await?;
    let other = message(base + 102, guild, channel, None);
    events_tx
        .send(Event::MessageCreate(Box::new(MessageCreate(other.clone()))))
        .await?;
    assert!(!exists(&pool, base + 100).await?);
    gate.add_permits(2);
    drop(events_tx);
    tokio::time::timeout(Duration::from_secs(5), worker).await???;
    assert_eq!(calls.load(Ordering::SeqCst), 2); // Original attachment plus the newly added one.
    let mut contexts = Vec::new();
    while let Some(context) = contexts_rx.recv().await {
        contexts.push(context);
    }
    contexts.sort_unstable_by_key(|context| context.message.id);
    assert_eq!(contexts.len(), 2);
    let context = &contexts[0];
    assert_eq!(context.message.id, original.id);
    assert_eq!(
        context.images[0].description.as_deref(),
        Some("cached caption")
    );
    let next = &contexts[1];
    assert_eq!(next.message.id, other.id);
    assert!(next.history.is_empty()); // The delete committed before the next snapshot.
    assert!(!exists(&pool, base + 100).await?);
    assert!(!exists(&pool, base + 101).await?);
    assert!(exists(&pool, base + 102).await?);
    cleanup(&pool, &[guild]).await?;
    pool.close().await;
    Ok(())
}

async fn exists(pool: &PgPool, id: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE id = $1)")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

async fn add_action(
    pool: &PgPool,
    guild: i64,
    channels: Option<Vec<i64>>,
    question: &str,
) -> Result<i32> {
    Ok(sqlx::query_scalar(
        "INSERT INTO message_actions (guild_id, only_channels, question, code)
         VALUES ($1, $2, $3, 'return true;') RETURNING id",
    )
    .bind(guild)
    .bind(channels)
    .bind(question)
    .fetch_one(pool)
    .await?)
}

async fn cleanup(pool: &PgPool, guilds: &[i64]) -> Result<()> {
    for query in [
        "DELETE FROM messages WHERE guild_id = ANY($1)",
        "DELETE FROM message_actions WHERE guild_id = ANY($1)",
    ] {
        sqlx::query(query).bind(guilds).execute(pool).await?;
    }
    Ok(())
}

async fn wait_until_stored(pool: &PgPool, id: i64) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !exists(pool, id).await? {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
