//! Broadcasters enroll their own channels from the bot account's chat.

use anyhow::Result;
use sqlx::PgPool;

use super::{
    ChatMessage,
    api::{Api, ApiError},
};

const LIMIT: i64 = 20;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Join,
    Leave,
    Help,
}

fn parse(text: &str) -> Option<Command> {
    let words: Vec<_> = text.split_whitespace().collect();
    match words.as_slice() {
        ["!join"] | ["!jeeves", "join"] => Some(Command::Join),
        ["!leave"] | ["!jeeves", "leave"] => Some(Command::Leave),
        ["!jeeves"] | ["!jeeves", "help"] => Some(Command::Help),
        ["!join" | "!leave", ..] | ["!jeeves", "join" | "leave", ..] => Some(Command::Help),
        _ => None,
    }
}

pub(super) async fn registered(pool: &PgPool, bot: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT channel_id FROM twitch_channels WHERE bot_user_id = $1 ORDER BY channel_id",
    )
    .bind(bot)
    .fetch_all(pool)
    .await?)
}

/// Serialize admission so concurrent broadcasters cannot exceed capacity.
async fn join(pool: &PgPool, bot: &str, channel: &str, login: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('twitch-channels:' || $1, 0))")
        .bind(bot)
        .execute(&mut *tx)
        .await?;
    let (count, exists): (i64, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(bool_or(channel_id = $2), false) FROM twitch_channels WHERE bot_user_id = $1",
    )
    .bind(bot)
    .bind(channel)
    .fetch_one(&mut *tx)
    .await?;
    if count >= LIMIT && !exists {
        return Ok(false);
    }
    sqlx::query("INSERT INTO twitch_channels (bot_user_id, channel_id, login) VALUES ($1, $2, $3) ON CONFLICT (bot_user_id, channel_id) DO UPDATE SET login = EXCLUDED.login")
        .bind(bot).bind(channel).bind(login).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

async fn leave(pool: &PgPool, bot: &str, channel: &str) -> Result<()> {
    sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1 AND channel_id = $2")
        .bind(bot)
        .bind(channel)
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) fn is_command(text: &str) -> bool {
    parse(text).is_some()
}

pub(super) async fn handle(pool: &PgPool, api: &Api, message: &ChatMessage) -> Result<()> {
    // The EventSub sender ID proves ownership: never accept a target channel argument,
    // a moderator badge, or a Shared Chat relay as authorization to enroll someone else.
    if message.broadcaster_user_id != api.user_id
        || !message.is_local()
        || message.chatter_user_id.is_empty()
        || message.chatter_user_id == api.user_id
    {
        return Ok(());
    }
    let reply = match parse(&message.message.text) {
        Some(Command::Join) => {
            let moderates = match api.moderates(&message.chatter_user_id).await {
                Ok(moderates) => moderates,
                Err(error) => {
                    tracing::warn!(?error, "Could not verify Twitch channel enrollment");
                    if error
                        .downcast_ref::<ApiError>()
                        .is_some_and(|error| error.0 == reqwest::StatusCode::UNAUTHORIZED)
                    {
                        return Err(error);
                    }
                    return api.say(&api.user_id, &format!("@{} I couldn't verify moderator access. Please try !join again shortly.", message.chatter_user_login)).await;
                }
            };
            if !moderates {
                "Make this bot account a moderator in your channel with /mod, then try !join again."
            } else if join(
                pool,
                &api.user_id,
                &message.chatter_user_id,
                &message.chatter_user_login,
            )
            .await?
            {
                "Your channel is registered! Jeeves will connect shortly. Use !addaction in your channel to add rules, or !leave here to stop."
            } else {
                "Jeeves has reached its 20-channel limit. Please contact the bot operator."
            }
        }
        Some(Command::Leave) => {
            leave(pool, &api.user_id, &message.chatter_user_id).await?;
            "Your channel is unregistered. Moderation will stop within a few seconds. Your rules and strikes are kept for !join."
        }
        Some(Command::Help) => {
            "To add Jeeves to your channel: /mod this bot account in your own chat, then !join here. Use !leave here to stop. These commands manage only your own channel; no channel name is needed."
        }
        None => return Ok(()),
    };
    api.say(
        &api.user_id,
        &format!("@{} {reply}", message.chatter_user_login),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::twitch::tests::{message, server};
    use serde_json::json;

    #[test]
    fn registration_never_accepts_a_target_channel() {
        for command in ["!join", "!jeeves join"] {
            assert_eq!(parse(command), Some(Command::Join));
            assert_eq!(parse(&format!("{command} other")), Some(Command::Help));
        }
        for command in ["!leave", "!jeeves leave"] {
            assert_eq!(parse(command), Some(Command::Leave));
            assert_eq!(parse(&format!("{command} other")), Some(Command::Help));
        }
        assert_eq!(parse("!joinery"), None);
        assert_eq!(parse("hello !join"), None);
    }

    #[tokio::test]
    async fn wrong_chat_and_shared_chat_cannot_register_channels() -> Result<()> {
        let pool =
            sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
        let api = Api::for_test("http://127.0.0.1:1".into());
        let mut message = message();
        message.message.text = "!join".into();
        handle(&pool, &api, &message).await?;
        message.broadcaster_user_id = api.user_id.clone();
        message.source_broadcaster_user_id = Some("other".into());
        handle(&pool, &api, &message).await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_permission_check_explains_retry_without_registering() -> Result<()> {
        let pool =
            sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
        let (base, requests) = server(vec![
            (503, json!({})),
            (200, json!({"data": [{"is_sent": true}]})),
        ]);
        let api = Api::for_test(base);
        let mut message = message();
        message.broadcaster_user_id = api.user_id.clone();
        message.message.text = "!join".into();
        handle(&pool, &api, &message).await?;
        assert!(
            requests.join().unwrap()[1].body["message"]
                .as_str()
                .unwrap()
                .contains("try !join again")
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires disposable TEST_DATABASE_URL"]
    async fn enrollment_verifies_moderator_access_persists_and_removes_only_sender() -> Result<()> {
        let pool = crate::db::connect(
            crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
            5,
        )
        .await?;
        let bot = format!("enrollment-bot-{}", std::process::id());
        sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
            .bind(&bot)
            .execute(&pool)
            .await?;
        let sent = (200, json!({"data": [{"is_sent": true}]}));
        let (base, requests) = server(vec![
            (200, json!({"data": [], "pagination": {}})),
            sent.clone(),
            (
                200,
                json!({"data": [{"broadcaster_id": "200"}], "pagination": {}}),
            ),
            sent.clone(),
            (
                200,
                json!({"data": [{"broadcaster_id": "200"}], "pagination": {}}),
            ),
            sent.clone(),
            sent.clone(),
            sent,
        ]);
        let mut api = Api::for_test(base);
        api.user_id = bot.clone();
        let mut message = message();
        message.broadcaster_user_id = bot.clone();
        message.message.text = "!join".into();
        handle(&pool, &api, &message).await?;
        assert!(registered(&pool, &bot).await?.is_empty());
        handle(&pool, &api, &message).await?;
        handle(&pool, &api, &message).await?;
        // A new pool (as on restart) sees exactly one durable registration.
        let other_pool = crate::db::connect(
            crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
            2,
        )
        .await?;
        assert_eq!(registered(&other_pool, &bot).await?, ["200"]);
        other_pool.close().await;
        assert!(join(&pool, &bot, "someone-else", "someone_else").await?);
        message.message.text = "!leave".into();
        handle(&pool, &api, &message).await?;
        handle(&pool, &api, &message).await?;
        assert_eq!(registered(&pool, &bot).await?, ["someone-else"]);
        let requests = requests.join().unwrap();
        assert!(requests[0].path.contains(&format!("user_id={bot}")));
        assert!(
            requests[1].body["message"]
                .as_str()
                .unwrap()
                .contains("moderator")
        );
        assert!(
            requests[3].body["message"]
                .as_str()
                .unwrap()
                .contains("registered")
        );
        sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
            .bind(&bot)
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires disposable TEST_DATABASE_URL"]
    async fn concurrent_admission_respects_capacity_and_existing_members() -> Result<()> {
        use futures_util::{StreamExt as _, stream};
        let pool = crate::db::connect(
            crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
            5,
        )
        .await?;
        let bot = format!("capacity-bot-{}", std::process::id());
        sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
            .bind(&bot)
            .execute(&pool)
            .await?;
        let results = stream::iter(0..25)
            .map(|id| {
                let pool = &pool;
                let bot = &bot;
                async move { join(pool, bot, &id.to_string(), "viewer").await }
            })
            .buffer_unordered(10)
            .collect::<Vec<_>>()
            .await;
        let admitted = results.into_iter().collect::<Result<Vec<_>>>()?;
        assert_eq!(
            admitted.iter().filter(|admitted| **admitted).count(),
            LIMIT as usize
        );
        let channels = registered(&pool, &bot).await?;
        assert_eq!(channels.len(), LIMIT as usize);
        assert!(join(&pool, &bot, &channels[0], "renamed_viewer").await?);
        assert!(!join(&pool, &bot, "new-channel", "new").await?);
        leave(&pool, &bot, &channels[0]).await?;
        assert!(join(&pool, &bot, "new-channel", "new").await?);
        assert!(registered(&pool, "unrelated-bot").await?.is_empty());
        sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
            .bind(&bot)
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }
}
