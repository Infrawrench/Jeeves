use anyhow::{Context, Result};
use jeeves::strike_actions::StoredStrike;
use sqlx::PgPool;
use twilight_model::{
    application::interaction::{
        Interaction, InteractionData, application_command::CommandOptionValue,
    },
    guild::Permissions,
    id::Id,
};

pub const MAX_REASON_LENGTH: u16 = 1000;

pub mod history;
pub mod manage;

// Validated manual input or an automatic rule outcome, with its deduplication source.
#[derive(Debug)]
pub(crate) struct NewStrike {
    pub guild_id: i64,
    pub channel_id: i64,
    pub user_id: i64,
    pub moderator_id: i64,
    pub reason: String,
    pub source: StrikeSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrikeSource {
    Interaction(i64),
    Message { message_id: i64, action_id: i32 },
}

pub(crate) struct RecordedStrike {
    pub strike: StoredStrike,
    pub created: bool,
}

pub async fn handle(
    moderation: &crate::moderation::Moderation,
    interaction: &Interaction,
) -> String {
    let strike = match parse(interaction) {
        Ok(strike) => strike,
        Err(message) => return message.into(),
    };
    match moderation.record_manual(&strike).await {
        Ok((_, failures)) => {
            let mut response = format!("Recorded a strike for <@{}>.", strike.user_id);
            if failures > 0 {
                response
                    .push_str(" Some strike notifications or actions failed; check the bot logs.");
            }
            response
        }
        Err(error) => {
            if let Some(error) = error.downcast_ref::<crate::moderation::StrikeCheckError>() {
                if matches!(error, crate::moderation::StrikeCheckError::Unavailable(_)) {
                    tracing::warn!(interaction_id = %interaction.id, ?error, "Could not verify strike hierarchy");
                }
                return error.to_string();
            }
            tracing::error!(interaction_id = %interaction.id, ?error, "Failed to record strike");
            "I couldn't save the strike. Please try again shortly.".into()
        }
    }
}

fn parse(interaction: &Interaction) -> Result<NewStrike, &'static str> {
    let guild_id = interaction
        .guild_id
        .ok_or("Strikes can only be issued in a server.")?;
    let member = interaction
        .member
        .as_ref()
        .ok_or("I couldn't verify your server permissions.")?;
    let permissions = member.permissions.unwrap_or_else(Permissions::empty);
    if !permissions.intersects(Permissions::MODERATE_MEMBERS | Permissions::ADMINISTRATOR) {
        return Err("You need the Moderate Members permission to issue strikes.");
    }
    let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_ref() else {
        return Err("Invalid strike command.");
    };
    let user_id = command
        .options
        .iter()
        .find_map(|option| match &option.value {
            CommandOptionValue::User(id) if option.name == "user" => Some(*id),
            _ => None,
        })
        .ok_or("Choose a server member to strike.")?;
    if !command
        .resolved
        .as_ref()
        .is_some_and(|resolved| resolved.members.contains_key(&user_id))
    {
        return Err("The selected user must be a member of this server.");
    }
    let reason = command
        .options
        .iter()
        .find_map(|option| match &option.value {
            CommandOptionValue::String(reason) if option.name == "reason" => Some(reason.trim()),
            _ => None,
        })
        .ok_or("Provide a reason for the strike.")?;
    if reason.is_empty() || reason.chars().count() > usize::from(MAX_REASON_LENGTH) {
        return Err("The reason must contain between 1 and 1000 characters.");
    }
    let moderator_id = member
        .user
        .as_ref()
        .ok_or("I couldn't identify the moderator.")?
        .id;
    let channel_id = interaction
        .channel
        .as_ref()
        .ok_or("I couldn't identify this channel.")?
        .id;
    Ok(NewStrike {
        guild_id: snowflake(guild_id)?,
        channel_id: snowflake(channel_id)?,
        user_id: snowflake(user_id)?,
        moderator_id: snowflake(moderator_id)?,
        reason: reason.to_owned(),
        source: StrikeSource::Interaction(snowflake(interaction.id)?),
    })
}

fn snowflake<T>(id: Id<T>) -> Result<i64, &'static str> {
    i64::try_from(id.get()).map_err(|_| "This Discord ID is outside the supported range.")
}

pub(crate) async fn record(pool: &PgPool, strike: &NewStrike) -> Result<RecordedStrike> {
    let (interaction_id, message_id, action_id) = match strike.source {
        StrikeSource::Interaction(id) => (Some(id), None, None),
        StrikeSource::Message {
            message_id,
            action_id,
        } => (None, Some(message_id), Some(action_id)),
    };
    let mut tx = pool.begin().await?;
    // Serialize inserts for a member before allocating an ID. Each rule's id
    // cutoff then includes every earlier committed strike, even during a burst.
    // The lock is released before any JavaScript, Jev, or Discord request.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(strike.guild_id.to_string())
        .bind(strike.user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query_as::<_, StoredStrike>(
        "INSERT INTO strikes (guild_id, channel_id, user_id, moderator_id, reason,
                              interaction_id, source_message_id, source_action_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT DO NOTHING
         RETURNING id, guild_id, channel_id, user_id, moderator_id, reason, created_at,
                   interaction_id, source_message_id, source_action_id",
    )
    .bind(strike.guild_id)
    .bind(strike.channel_id)
    .bind(strike.user_id)
    .bind(strike.moderator_id)
    .bind(&strike.reason)
    .bind(interaction_id)
    .bind(message_id)
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await
    .context("failed to insert strike")?;
    let created = inserted.is_some();
    let saved = match inserted {
        Some(saved) => saved,
        None => {
            sqlx::query_as::<_, StoredStrike>(
                "SELECT id, guild_id, channel_id, user_id, moderator_id, reason, created_at,
                    interaction_id, source_message_id, source_action_id FROM strikes
             WHERE interaction_id = $1 OR
                   (guild_id = $2 AND source_message_id = $3 AND source_action_id = $4)",
            )
            .bind(interaction_id)
            .bind(strike.guild_id)
            .bind(message_id)
            .bind(action_id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    Ok(RecordedStrike {
        strike: saved,
        created,
    })
}

#[cfg(test)]
pub(crate) mod tests;
