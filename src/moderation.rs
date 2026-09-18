use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt as _, stream};
use jeeves::{
    message_actions::{self, ActionResult, MessageActionOutcome, MessageContext},
    strike_actions::{self, StoredStrike},
    typesafe,
};
use sqlx::PgPool;
use twilight_http::{Client, error::ErrorType, request::AuditLogReason as _};
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
    },
};

use crate::strikes::{self, NewStrike, StrikeSource};

mod hierarchy;
pub(crate) use hierarchy::StrikeCheckError;

#[derive(Clone)]
pub struct Moderation {
    pool: PgPool,
    http: Arc<Client>,
    jev: typesafe::Client,
    bot_user_id: Id<UserMarker>,
}

#[derive(Debug, PartialEq, Eq)]
enum Removal {
    Kick(String),
    Ban(String),
}

#[derive(Debug, Default)]
struct Plan {
    removal: Option<Removal>,
    strikes: Vec<(i32, String)>,
    failures: usize,
}

impl Plan {
    fn include(&mut self, results: Vec<ActionResult<MessageActionOutcome>>) {
        for result in results {
            match result.result {
                Ok(MessageActionOutcome::Ban(reason)) => {
                    if !matches!(self.removal, Some(Removal::Ban(_))) {
                        self.removal = Some(Removal::Ban(reason));
                    }
                }
                Ok(MessageActionOutcome::Kick(reason)) => {
                    if self.removal.is_none() {
                        self.removal = Some(Removal::Kick(reason));
                    }
                }
                Ok(MessageActionOutcome::Strike(reason)) => {
                    self.strikes.push((result.action_id, reason))
                }
                Ok(MessageActionOutcome::Ignore) => {}
                Err(_) => self.failures += 1,
            }
        }
    }
}

impl Moderation {
    pub fn new(
        pool: PgPool,
        http: Arc<Client>,
        jev: typesafe::Client,
        bot_user_id: Id<UserMarker>,
    ) -> Self {
        Self {
            pool,
            http,
            jev,
            bot_user_id,
        }
    }

    pub async fn process_message(&self, context: MessageContext) -> Result<()> {
        // Notices are still stored as history, but must never moderate themselves.
        if context.message.author.id == self.bot_user_id {
            return Ok(());
        }
        let Some(guild_id) = context.message.guild_id else {
            return Ok(());
        };
        let channel_id = context.message.channel_id;
        let user_id = context.message.author.id;
        let message_id = context.message.id;
        let report = message_actions::process_message(context, self.jev.clone()).await;
        let mut plan = Plan::default();
        plan.include(report.results);
        if !plan.strikes.is_empty()
            && let Err(error) =
                hierarchy::check(&self.http, guild_id, self.bot_user_id, user_id).await
        {
            if matches!(error, StrikeCheckError::Protected) {
                tracing::debug!(%guild_id, %user_id, "Skipped automatic strikes for protected member");
            } else {
                plan.failures += 1;
                tracing::warn!(%guild_id, %user_id, ?error, "Skipped strikes because hierarchy could not be verified");
            }
            plan.strikes.clear();
        }
        let mut created = Vec::new();
        // Persist every strike, even when the same batch also asks for a removal.
        // One failed insert must not discard the remaining strike outcomes.
        for (action_id, reason) in std::mem::take(&mut plan.strikes) {
            let strike = NewStrike {
                guild_id: snowflake(guild_id)?,
                channel_id: snowflake(channel_id)?,
                user_id: snowflake(user_id)?,
                moderator_id: snowflake(self.bot_user_id)?,
                reason: normalized_reason(&reason, action_id),
                source: StrikeSource::Message {
                    message_id: snowflake(message_id)?,
                    action_id,
                },
            };
            match strikes::record(&self.pool, &strike).await {
                Ok(saved) if saved.created => created.push(saved.strike),
                Ok(_) => {} // Replayed message/action; never add another strike or rerun its rules.
                Err(error) => {
                    plan.failures += 1;
                    tracing::error!(%message_id, action_id, ?error, "Failed to record automatic strike");
                }
            }
        }
        let count = created.len();
        let ((), feedback_failures) = tokio::join!(
            self.evaluate_strikes(created, &mut plan),
            self.notify_strikes(channel_id, user_id, Some(message_id), count),
        );
        plan.failures += feedback_failures;
        self.apply_removal(guild_id, user_id, &mut plan).await;
        ensure!(
            plan.failures == 0,
            "{} moderation steps failed for message {message_id}",
            plan.failures
        );
        Ok(())
    }

    /// Shared manual strike path. A saved strike stays confirmed even if a rule
    /// or Discord request fails; retries do not trigger the same strike twice.
    pub(crate) async fn record_manual(&self, strike: &NewStrike) -> Result<(i64, usize)> {
        hierarchy::check(
            &self.http,
            discord_id(strike.guild_id)?,
            self.bot_user_id,
            discord_id(strike.user_id)?,
        )
        .await?;
        let saved = strikes::record(&self.pool, strike).await?;
        let id = saved.strike.id;
        if !saved.created {
            return Ok((id, 0));
        }
        let guild_id = discord_id(saved.strike.guild_id)?;
        let channel_id = discord_id(saved.strike.channel_id)?;
        let user_id = discord_id(saved.strike.user_id)?;
        let mut plan = Plan::default();
        let ((), feedback_failures) = tokio::join!(
            self.evaluate_strikes(vec![saved.strike], &mut plan),
            self.notify_strikes(channel_id, user_id, None, 1),
        );
        plan.failures += feedback_failures;
        self.apply_removal(guild_id, user_id, &mut plan).await;
        Ok((id, plan.failures))
    }

    /// Feedback never rolls back a strike or prevents escalation. Coalesce all
    /// strikes from one message, and do nothing for duplicate events.
    async fn notify_strikes(
        &self,
        channel_id: Id<ChannelMarker>,
        user_id: Id<UserMarker>,
        message_id: Option<Id<MessageMarker>>,
        count: usize,
    ) -> usize {
        if count == 0 {
            return 0;
        }
        let delete = async {
            let Some(message_id) = message_id else {
                return 0;
            };
            match self.http.delete_message(channel_id, message_id).await {
                Ok(_) => 0,
                Err(error) if matches!(error.kind(), ErrorType::Response { status, .. } if status.get() == 404) => {
                    0
                }
                Err(error) => {
                    tracing::warn!(%channel_id, %message_id, ?error, "Could not delete struck message");
                    1
                }
            }
        };
        let notify = async {
            let strikes = if count == 1 {
                "a strike".to_owned()
            } else {
                format!("{count} strikes")
            };
            let content =
                format!("<@{user_id}>, you received {strikes}. Use /strikes to view the reasons.");
            let mentions = AllowedMentions {
                users: vec![user_id],
                ..AllowedMentions::default()
            };
            match self
                .http
                .create_message(channel_id)
                .content(&content)
                .allowed_mentions(Some(&mentions))
                .await
            {
                Ok(_) => 0,
                Err(error) => {
                    tracing::warn!(%channel_id, %user_id, ?error, "Could not send strike notice");
                    1
                }
            }
        };
        let (delete_failures, notify_failures) = tokio::join!(delete, notify);
        delete_failures + notify_failures
    }

    async fn evaluate_strikes(&self, strikes: Vec<StoredStrike>, plan: &mut Plan) {
        let reports = stream::iter(strikes)
            .map(|strike| async move {
                let id = strike.id;
                let result = match strike_actions::load_context(&self.pool, strike).await {
                    Ok(context) => {
                        Ok(strike_actions::process_strike(context, self.jev.clone()).await)
                    }
                    Err(error) => Err(error),
                };
                (id, result)
            })
            .buffered(8)
            .collect::<Vec<_>>()
            .await;
        for (strike_id, report) in reports {
            match report {
                Ok(report) => plan.include(report.results),
                Err(error) => {
                    plan.failures += 1;
                    tracing::error!(strike_id, ?error, "Failed to process strike actions");
                }
            }
        }
    }

    async fn apply_removal(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
        plan: &mut Plan,
    ) {
        let Some(removal) = plan.removal.take() else {
            return;
        };
        if let Err(error) = self.remove(guild_id, user_id, &removal).await {
            plan.failures += 1;
            tracing::error!(%guild_id, %user_id, ?error, "Failed to apply moderation removal");
        }
    }

    async fn remove(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
        removal: &Removal,
    ) -> Result<()> {
        match removal {
            Removal::Ban(reason) => {
                let reason: String = reason.chars().take(512).collect();
                self.http
                    .create_ban(guild_id, user_id)
                    .delete_message_seconds(0)
                    .reason(&reason)
                    .await
                    .context("failed to ban member")?;
            }
            Removal::Kick(reason) => {
                let reason: String = reason.chars().take(512).collect();
                self.http
                    .remove_guild_member(guild_id, user_id)
                    .reason(&reason)
                    .await
                    .context("failed to kick member")?;
            }
        }
        Ok(())
    }
}

fn normalized_reason(reason: &str, action_id: i32) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        format!("Message action #{action_id}")
    } else {
        reason
            .chars()
            .take(usize::from(strikes::MAX_REASON_LENGTH))
            .collect()
    }
}

fn snowflake<T>(id: Id<T>) -> Result<i64> {
    i64::try_from(id.get()).context("Discord ID exceeds PostgreSQL BIGINT range")
}

fn discord_id<T>(value: i64) -> Result<Id<T>> {
    let value = u64::try_from(value).context("invalid stored Discord ID")?;
    Id::new_checked(value).context("invalid stored Discord ID")
}

#[cfg(test)]
mod tests;
