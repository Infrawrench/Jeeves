use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sqlx::PgPool;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::{interval, timeout},
};
use twilight_gateway::{
    CloseFrame, Event, EventTypeFlags, Intents, Shard, ShardId, ShardState, StreamExt as _,
};
use twilight_http::Client;
use twilight_model::gateway::{
    payload::outgoing::UpdatePresence,
    presence::{ActivityType, MinimalActivity, Status},
};

pub async fn run(
    token: String,
    pool: PgPool,
    gemini: crate::gemini::Gemini,
    jev: jeeves::typesafe::Client,
) -> Result<()> {
    let http = Arc::new(Client::new(token.clone()));
    let application = http
        .current_user_application()
        .await
        .context("failed to authenticate with Discord")?
        .model()
        .await?;
    crate::commands::register(&http, application.id).await?;
    let bot_user_id = http.current_user().await?.model().await?.id;
    let moderation =
        crate::moderation::Moderation::new(pool.clone(), http.clone(), jev.clone(), bot_user_id);

    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let mut guilds = HashSet::new();
    let mut displayed_guild_count = None;
    let mut presence_tick = interval(Duration::from_secs(5));
    let (message_tx, message_rx) = mpsc::channel(1024);
    let mut ingestion = tokio::spawn(crate::messages::ingest(
        pool.clone(),
        message_rx,
        gemini.clone(),
        moderation.clone(),
    ));
    let mut ingestion_finished = false;
    let mut handlers = JoinSet::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let result = loop {
        tokio::select! {
            result = &mut shutdown => break result,
            result = &mut ingestion => {
                ingestion_finished = true;
                break Err(worker_error("message storage", result));
            }
            Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                log_handler_result(result);
            }
            _ = presence_tick.tick(), if shard.state() == ShardState::Active
                && displayed_guild_count != Some(guilds.len()) => {
                // Space updates even after delayed ticks or reconnects.
                presence_tick.reset();
                let count = guilds.len();
                let noun = if count == 1 { "server" } else { "servers" };
                let activity = MinimalActivity {
                    kind: ActivityType::Playing,
                    name: format!("Moderating in {count} {noun}"),
                    url: None,
                };
                let presence = UpdatePresence::new(vec![activity.into()], false, None, Status::Online)
                    .expect("the guild-count presence always contains one activity");
                shard.command(&presence);
                displayed_guild_count = Some(count);
                tracing::info!(guild_count = count, "Queued guild-count playing presence");
            }
            event = shard.next_event(EventTypeFlags::all()) => {
                match event {
                    Some(Ok(Event::Ready(ready))) => {
                        guilds = ready.guilds.iter().map(|guild| guild.id).collect();
                        displayed_guild_count = None;
                        tracing::info!(user = %ready.user.name, "Discord bot connected");
                    }
                    Some(Ok(Event::Resumed)) => {
                        displayed_guild_count = None;
                    }
                    Some(Ok(Event::GuildCreate(guild))) => {
                        guilds.insert(guild.id());
                    }
                    Some(Ok(Event::GuildDelete(guild))) => {
                        // Temporary outages do not change guild membership.
                        if guild.unavailable != Some(true) {
                            guilds.remove(&guild.id);
                        }
                    }
                    Some(Ok(Event::InteractionCreate(event))) => {
                        let http = Arc::clone(&http);
                        let pool = pool.clone();
                        let moderation = moderation.clone();
                        let gemini = gemini.clone();
                        let jev = jev.clone();
                        handlers.spawn(async move {
                            crate::commands::handle(&http, &pool, &moderation, &gemini, &jev, event.0).await
                        });
                    }
                    Some(Ok(event @ (Event::MessageCreate(_) | Event::MessageUpdate(_) | Event::MessageDelete(_) | Event::MessageDeleteBulk(_)))) => {
                        if message_tx.send(event).await.is_err() {
                            break Err(anyhow::anyhow!("message storage worker disconnected"));
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        // Deserialization errors can contain raw interaction tokens.
                        tracing::warn!("Gateway receive error; continuing to poll the shard");
                    }
                    None => break Err(anyhow::anyhow!("Discord gateway stopped unexpectedly")),
                }
            }
        }
    };

    tracing::info!("Shutting down");
    drop(message_tx);
    shard.close(CloseFrame::NORMAL);
    let _ = timeout(Duration::from_secs(5), async {
        while let Some(event) = shard.next_event(EventTypeFlags::all()).await {
            if matches!(event, Ok(Event::GatewayClose(_))) {
                break;
            }
        }
    })
    .await;

    if !ingestion_finished {
        stop_worker(&mut ingestion).await;
    }

    if timeout(Duration::from_secs(15), async {
        while let Some(result) = handlers.join_next().await {
            log_handler_result(result);
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("Aborting handlers after shutdown deadline");
        handlers.shutdown().await;
    }
    result
}

fn worker_error(name: &str, result: Result<Result<()>, tokio::task::JoinError>) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow::anyhow!("{name} worker stopped unexpectedly"),
        Ok(Err(error)) => error.context(format!("{name} worker failed")),
        Err(error) => anyhow::Error::new(error).context(format!("{name} worker panicked")),
    }
}

async fn stop_worker(worker: &mut JoinHandle<Result<()>>) {
    match timeout(Duration::from_secs(15), &mut *worker).await {
        Ok(Ok(Ok(()))) => {}
        Ok(result) => {
            tracing::error!(error = ?worker_error("background", result), "Worker failed during shutdown")
        }
        Err(_) => {
            tracing::warn!("Aborting worker after shutdown deadline");
            worker.abort();
            let _ = worker.await;
        }
    }
}

fn log_handler_result(result: Result<Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(?error, "Interaction handler failed"),
        Err(error) => tracing::error!(?error, "Interaction task failed"),
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
