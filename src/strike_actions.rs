//! Evaluate configured rules after a member receives a strike.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{
    message_actions::{ActionResult, MessageAction, MessageActionOutcome, javascript, run_actions},
    typesafe,
};

pub type StrikeAction = MessageAction;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StoredStrike {
    pub id: i64,
    pub guild_id: i64,
    pub channel_id: i64,
    pub user_id: i64,
    pub moderator_id: i64,
    pub reason: String,
    pub created_at: OffsetDateTime,
    pub interaction_id: Option<i64>,
    pub source_message_id: Option<i64>,
    pub source_action_id: Option<i32>,
}

#[derive(Debug)]
pub struct StrikeContext {
    pub strike: StoredStrike,
    /// The member's earlier strikes in this guild, in insertion order.
    pub history: Vec<StoredStrike>,
    pub actions: Vec<StrikeAction>,
}

#[derive(Debug)]
pub struct StrikeActionContext {
    pub strike: StoredStrike,
    pub history: Vec<StoredStrike>,
}

#[derive(Debug)]
#[must_use]
pub struct StrikeActionReport {
    pub strike_id: i64,
    pub results: Vec<ActionResult<MessageActionOutcome>>,
}

pub async fn load_context(pool: &PgPool, strike: StoredStrike) -> Result<StrikeContext> {
    let history = sqlx::query_as::<_, StoredStrike>(
        "SELECT id, guild_id, channel_id, user_id, moderator_id, reason, created_at,
                interaction_id, source_message_id, source_action_id
         FROM strikes WHERE guild_id = $1 AND user_id = $2 AND id < $3 ORDER BY id",
    )
    .bind(strike.guild_id)
    .bind(strike.user_id)
    .bind(strike.id)
    .fetch_all(pool);
    let actions = sqlx::query_as::<_, StrikeAction>(
        "SELECT id, guild_id, only_channels, question, code FROM strike_actions
         WHERE guild_id = $1 AND (only_channels IS NULL OR $2 = ANY(only_channels)) ORDER BY id",
    )
    .bind(strike.guild_id)
    .bind(strike.channel_id)
    .fetch_all(pool);
    let (history, actions) = tokio::try_join!(history, actions)
        .context("failed to load strike history and configured actions")?;
    Ok(StrikeContext {
        strike,
        history,
        actions,
    })
}

pub async fn process_strike(context: StrikeContext, jev: typesafe::Client) -> StrikeActionReport {
    let StrikeContext {
        strike,
        history,
        actions,
    } = context;
    let strike_id = strike.id;
    let context = Arc::new(StrikeActionContext { strike, history });
    let results = run_actions(context, actions, move |context, action| {
        process_action(context, action, jev.clone())
    })
    .await;
    StrikeActionReport { strike_id, results }
}

pub async fn process_action(
    context: Arc<StrikeActionContext>,
    action: StrikeAction,
    jev: typesafe::Client,
) -> Result<MessageActionOutcome> {
    if let Some(code) = action.code {
        let outcome = javascript::execute_with_input(code, action.question, move || {
            Ok(serde_json::to_vec(&strike_values(&context))?)
        })
        .await?;
        ensure!(
            !matches!(outcome, MessageActionOutcome::Strike(_)),
            "strike actions must return BAN, KICK, or null; recursive strikes are not supported"
        );
        return Ok(outcome);
    }
    let question = typesafe::Question::score(
        format!(
            "What is the appropriate action for this strike rule based on the user's strike history? Choose no action if the rule does not match or specifies no action: {}",
            action.question,
        ),
        ["ban", "kick", "no action"],
    )?;
    let mut strikes = strike_values(&context);
    let current = strikes.pop().expect("strike input contains the new strike");
    let state = json!({
        "current_strike": current,
        "history": strikes,
        "total_strikes": context.history.len() + 1,
    });
    let evaluation = jev.ask(&state, &question).await?;
    let (&level, _) = evaluation
        .answer
        .probabilities
        .iter()
        .max_by(|(left_level, left), (right_level, right)| {
            left.get()
                .total_cmp(&right.get())
                .then_with(|| left_level.cmp(right_level))
        })
        .context("Jev returned no strike action probabilities")?;
    Ok(match level {
        0 => MessageActionOutcome::Ban(action.question),
        1 => MessageActionOutcome::Kick(action.question),
        2 => MessageActionOutcome::Ignore,
        _ => anyhow::bail!("Jev returned an unknown strike action level: {level}"),
    })
}

fn strike_values(context: &StrikeActionContext) -> Vec<Value> {
    context
        .history
        .iter()
        .chain(std::iter::once(&context.strike))
        .map(|strike| {
            json!({
                "id": strike.id.to_string(),
                "guild_id": strike.guild_id.to_string(),
                "channel_id": strike.channel_id.to_string(),
                "user_id": strike.user_id.to_string(),
                "moderator_id": strike.moderator_id.to_string(),
                "reason": strike.reason,
                "created_at": strike.created_at.unix_timestamp_nanos() / 1_000_000,
                "interaction_id": strike.interaction_id.map(|id| id.to_string()),
                "source_message_id": strike.source_message_id.map(|id| id.to_string()),
                "source_action_id": strike.source_action_id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
