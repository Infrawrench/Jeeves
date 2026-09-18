use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

use super::{Action, ChatMessage};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Rule {
    pub id: i64,
    pub condition: String,
    pub action: String,
    pub timeout_seconds: Option<i32>,
    pub strike_threshold: Option<i32>,
}

impl Rule {
    pub fn outcome(&self) -> Result<Action> {
        match self.action.as_str() {
            "delete" => Ok(Action::Delete),
            "strike" => Ok(Action::Strike),
            "ban" => Ok(Action::Ban),
            "timeout" => Ok(Action::Timeout(
                self.timeout_seconds.context("missing timeout duration")?,
            )),
            _ => anyhow::bail!("invalid stored Twitch action"),
        }
    }
}

pub async fn rules(pool: &PgPool, channel: &str) -> Result<Vec<Rule>> {
    Ok(sqlx::query_as("SELECT id, condition, action, timeout_seconds, strike_threshold FROM twitch_rules WHERE channel_id = $1 ORDER BY id")
        .bind(channel).fetch_all(pool).await?)
}

pub async fn add_rule(
    pool: &PgPool,
    message: &ChatMessage,
    condition: &str,
    action: &Action,
    threshold: Option<i32>,
) -> Result<i64> {
    Ok(sqlx::query_scalar("INSERT INTO twitch_rules (channel_id, condition, action, timeout_seconds, strike_threshold, created_by_message_id) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (channel_id, created_by_message_id) DO UPDATE SET created_by_message_id = EXCLUDED.created_by_message_id RETURNING id")
        .bind(&message.broadcaster_user_id).bind(condition).bind(action.name())
        .bind(if let Action::Timeout(seconds) = action { Some(*seconds) } else { None })
        .bind(threshold).bind(&message.message_id).fetch_one(pool).await?)
}

pub async fn claim(pool: &PgPool, channel: &str, id: &str) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO twitch_receipts (channel_id, event_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(channel)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn prune_receipts(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM twitch_receipts WHERE received_at < now() - interval '1 day'")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn save_message(
    pool: &PgPool,
    message: &ChatMessage,
    timestamp: OffsetDateTime,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('twitch-history:' || $1, 0))")
        .bind(&message.broadcaster_user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO twitch_messages (channel_id, id, author_id, content, timestamp) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING")
        .bind(&message.broadcaster_user_id).bind(&message.message_id).bind(&message.chatter_user_id)
        .bind(&message.message.text).bind(timestamp).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM twitch_messages WHERE channel_id = $1 AND id IN (SELECT id FROM twitch_messages WHERE channel_id = $1 ORDER BY timestamp DESC, id DESC OFFSET 500)")
        .bind(&message.broadcaster_user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn history(pool: &PgPool, channel: &str) -> Result<Vec<Value>> {
    let rows: Vec<(String, String, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, author_id, content, timestamp FROM twitch_messages WHERE channel_id = $1 ORDER BY timestamp DESC, id DESC LIMIT 500")
        .bind(channel).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(id, author, content, timestamp)| {
            json!({
                "id": id, "author_id": author, "content": content,
                "timestamp": timestamp.unix_timestamp_nanos() / 1_000_000,
            })
        })
        .collect())
}

pub async fn clear_messages(
    pool: &PgPool,
    channel: &str,
    message_id: Option<&str>,
    user_id: Option<&str>,
    through: OffsetDateTime,
) -> Result<()> {
    sqlx::query("DELETE FROM twitch_messages WHERE channel_id = $1 AND ($2::text IS NULL OR id = $2) AND ($3::text IS NULL OR author_id = $3) AND timestamp <= $4")
        .bind(channel).bind(message_id).bind(user_id).bind(through).execute(pool).await?;
    Ok(())
}

/// Serialize strike insertion and counting for this channel/user across instances.
/// No database connection is held during Jev or Twitch requests.
pub async fn record_strikes(
    pool: &PgPool,
    message: &ChatMessage,
    rules: &[&Rule],
) -> Result<(u64, i64)> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('twitch-strikes:' || $1 || ':' || $2, 0))",
    )
    .bind(&message.broadcaster_user_id)
    .bind(&message.chatter_user_id)
    .execute(&mut *tx)
    .await?;
    let mut added = 0;
    for rule in rules {
        added += sqlx::query("INSERT INTO twitch_strikes (channel_id, user_id, reason, source_message_id, source_rule_id) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING")
            .bind(&message.broadcaster_user_id).bind(&message.chatter_user_id).bind(&rule.condition)
            .bind(&message.message_id).bind(rule.id).execute(&mut *tx).await?.rows_affected();
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM twitch_strikes WHERE channel_id = $1 AND user_id = $2 AND removed_at IS NULL")
        .bind(&message.broadcaster_user_id).bind(&message.chatter_user_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok((added, count))
}

pub async fn strikes(
    pool: &PgPool,
    channel: &str,
    user: &str,
    after: i64,
) -> Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as("SELECT id, reason FROM twitch_strikes WHERE channel_id = $1 AND user_id = $2 AND removed_at IS NULL AND id > $3 ORDER BY id LIMIT 4")
        .bind(channel).bind(user).bind(after).fetch_all(pool).await?)
}

pub async fn remove_strike(pool: &PgPool, channel: &str, id: i64) -> Result<bool> {
    Ok(sqlx::query("UPDATE twitch_strikes SET removed_at = now() WHERE channel_id = $1 AND id = $2 AND removed_at IS NULL")
        .bind(channel).bind(id).execute(pool).await?.rows_affected() == 1)
}
