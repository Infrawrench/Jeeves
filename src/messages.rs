use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use futures_util::{FutureExt as _, StreamExt as _, stream};
use jeeves::{
    images::ImageResult,
    message_actions::{self, MessageContext},
};
use sqlx::{PgPool, types::Json};
use time::OffsetDateTime;
use tokio::{sync::mpsc, task::JoinSet};
use twilight_gateway::Event;
use twilight_model::{
    channel::{Attachment, Message},
    id::Id,
    util::Timestamp,
};

use crate::gemini::{Gemini, MAX_IMAGE_BYTES};

/// Image preparation and database writes follow gateway order. Action processing
/// runs alongside each write, using the same completed image results.
pub async fn ingest(
    pool: PgPool,
    events: mpsc::Receiver<Event>,
    gemini: Gemini,
    moderation: crate::moderation::Moderation,
) -> Result<()> {
    ingest_with_handler(
        pool,
        events,
        move |url: String, mime_type: String| {
            let gemini = gemini.clone();
            async move { gemini.describe(&url, &mime_type).await }
        },
        move |context| {
            let moderation = moderation.clone();
            async move { moderation.process_message(context).await }
        },
    )
    .await
}

const MAX_ACTION_TASKS: usize = 32;
const MAX_IMAGE_REQUESTS: usize = 4;
const IMAGE_TIMEOUT: Duration = Duration::from_secs(90);

async fn ingest_with_handler<D, ImageFuture, F, Fut>(
    pool: PgPool,
    mut events: mpsc::Receiver<Event>,
    describe: D,
    handler: F,
) -> Result<()>
where
    D: Fn(String, String) -> ImageFuture + Clone + Send + Sync + 'static,
    ImageFuture: Future<Output = Result<String>> + Send + 'static,
    F: Fn(MessageContext) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut actions = JoinSet::new();
    loop {
        let event = tokio::select! {
            Some(result) = actions.join_next(), if !actions.is_empty() => {
                log_action_result(result);
                continue;
            }
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
        };
        match event {
            Event::MessageCreate(message) => {
                if message.guild_id.is_none() {
                    continue;
                }
                if actions.len() >= MAX_ACTION_TASKS
                    && let Some(result) = actions.join_next().await
                {
                    log_action_result(result);
                }
                // Complete image work before fetching the history/action snapshot.
                let images = transcribe_images(&message.0, &[], describe.clone()).await?;
                match message_actions::load_context(&pool, &message.0, images.clone()).await {
                    Ok(Some(context)) => {
                        let handler = handler.clone();
                        actions.spawn(async move {
                            let id = context.message.id;
                            handler(context).await.with_context(|| {
                                format!("message action handler failed for message {id}")
                            })
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(message_id = %message.id, ?error, "Skipping message action task; context query failed");
                    }
                }
                // Runs concurrently with the spawned processor. Await this write before
                // handling the next event so edits/deletes cannot overtake the insert.
                save(&pool, &message.0, &images, false).await?;
            }
            Event::MessageUpdate(message) => {
                // Reuse completed descriptions for unchanged attachments. Missing rows
                // were deleted/pruned and must not be recreated by a late edit.
                if let Some(cached) = cached_images(&pool, &message.0).await? {
                    let images = transcribe_images(&message.0, &cached, describe.clone()).await?;
                    save(&pool, &message.0, &images, true).await?;
                }
            }
            Event::MessageDelete(message) => {
                delete(
                    &pool,
                    snowflake(message.channel_id)?,
                    &[snowflake(message.id)?],
                )
                .await?;
            }
            Event::MessageDeleteBulk(message) => {
                let ids = message
                    .ids
                    .into_iter()
                    .map(snowflake)
                    .collect::<Result<Vec<_>>>()?;
                delete(&pool, snowflake(message.channel_id)?, &ids).await?;
            }
            _ => {}
        }
    }
    while let Some(result) = actions.join_next().await {
        log_action_result(result);
    }
    Ok(())
}

fn log_action_result(result: Result<Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(?error, "Message action handler failed"),
        Err(error) => tracing::error!(?error, "Message action task failed"),
    }
}

async fn transcribe_images<D, Fut>(
    message: &Message,
    cached: &[ImageResult],
    describe: D,
) -> Result<Vec<ImageResult>>
where
    D: Fn(String, String) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let images = message
        .attachments
        .iter()
        .filter_map(|attachment| image_mime(attachment).map(|mime| (attachment, mime)))
        .map(|(attachment, mime)| {
            let attachment_id = snowflake(attachment.id)?;
            let cached_description = cached
                .iter()
                .find(|previous| {
                    previous.attachment_id == attachment_id && previous.mime_type == mime
                })
                .and_then(|previous| previous.description.clone());
            Ok((
                ImageResult {
                    attachment_id,
                    url: attachment.url.clone(),
                    mime_type: mime.into(),
                    description: None,
                    description_error: None,
                },
                attachment.size,
                cached_description,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(stream::iter(images).map(move |(mut image, size, cached_description)| {
        let describe = describe.clone();
        async move {
            if size > MAX_IMAGE_BYTES as u64 {
                image.description_error = Some("image exceeds 12 MiB download limit".into());
                return image;
            }
            if let Some(description) = cached_description {
                image.description = Some(description);
                return image;
            }
            let result = tokio::time::timeout(IMAGE_TIMEOUT, describe(image.url.clone(), image.mime_type.clone()))
                .await.context("image description timed out")
                .and_then(|result| result);
            match result {
                Ok(description) => image.description = Some(description),
                Err(error) => {
                    tracing::warn!(attachment_id = image.attachment_id, %error, "Image description failed");
                    image.description_error = Some(error.to_string());
                }
            }
            image
        }
        .boxed()
    }).buffered(MAX_IMAGE_REQUESTS).collect().await)
}

async fn cached_images(pool: &PgPool, message: &Message) -> Result<Option<Vec<ImageResult>>> {
    let Some(guild_id) = message.guild_id else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar::<_, Json<Vec<ImageResult>>>(
        "SELECT images FROM messages WHERE id = $1 AND guild_id = $2 AND channel_id = $3",
    )
    .bind(snowflake(message.id)?)
    .bind(snowflake(guild_id)?)
    .bind(snowflake(message.channel_id)?)
    .fetch_optional(pool)
    .await?
    .map(|images| images.0))
}

async fn save(
    pool: &PgPool,
    message: &Message,
    images: &[ImageResult],
    update: bool,
) -> Result<()> {
    let Some(guild_id) = message.guild_id else {
        return Ok(());
    };
    let id = snowflake(message.id)?;
    let guild_id = snowflake(guild_id)?;
    let channel_id = snowflake(message.channel_id)?;
    if update {
        sqlx::query(
            "UPDATE messages SET content = $2, edited_timestamp = $3, images = $4
             WHERE id = $1 AND guild_id = $5 AND channel_id = $6",
        )
        .bind(id)
        .bind(&message.content)
        .bind(message.edited_timestamp.map(timestamp).transpose()?)
        .bind(Json(images))
        .bind(guild_id)
        .bind(channel_id)
        .execute(pool)
        .await
        .context("failed to update Discord message")?;
    } else {
        // One atomic write; the database's insert trigger owns the retention lock.
        sqlx::query(
            "INSERT INTO messages (id, guild_id, channel_id, author_id, content, timestamp, edited_timestamp, images)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
        )
        .bind(id).bind(guild_id).bind(channel_id).bind(snowflake(message.author.id)?)
        .bind(&message.content).bind(timestamp(message.timestamp)?)
        .bind(message.edited_timestamp.map(timestamp).transpose()?).bind(Json(images))
        .execute(pool).await.context("failed to store Discord message")?;
    }
    Ok(())
}

pub(crate) async fn delete(pool: &PgPool, channel_id: i64, ids: &[i64]) -> Result<()> {
    sqlx::query("DELETE FROM messages WHERE channel_id = $1 AND id = ANY($2)")
        .bind(channel_id)
        .bind(ids)
        .execute(pool)
        .await
        .context("failed to delete Discord messages")?;
    Ok(())
}

fn snowflake<T>(id: Id<T>) -> Result<i64> {
    i64::try_from(id.get()).context("Discord ID exceeds PostgreSQL BIGINT range")
}

fn timestamp(value: Timestamp) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value.as_micros()) * 1000)
        .context("invalid Discord timestamp")
}

fn image_mime(attachment: &Attachment) -> Option<&'static str> {
    let mime = attachment
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()?
        .trim();
    match mime {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/heic" => Some("image/heic"),
        "image/heif" => Some("image/heif"),
        "" => match attachment
            .filename
            .rsplit('.')
            .next()?
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "webp" => Some("image/webp"),
            "heic" => Some("image/heic"),
            "heif" => Some("image/heif"),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests;
