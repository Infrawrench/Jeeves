use anyhow::{Context, Result};
use twilight_http::Client;
use twilight_model::{
    guild::{Guild, Role},
    id::{
        Id,
        marker::{GuildMarker, RoleMarker, UserMarker},
    },
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum StrikeCheckError {
    #[error(
        "I can't strike the server owner, myself, or members whose highest role is equal to or above mine. No strike was added."
    )]
    Protected,
    #[error(
        "I couldn't verify this member's role hierarchy. No strike was added. Please try again shortly."
    )]
    Unavailable(#[source] anyhow::Error),
}

pub(super) async fn check(
    http: &Client,
    guild_id: Id<GuildMarker>,
    bot_id: Id<UserMarker>,
    user_id: Id<UserMarker>,
) -> Result<(), StrikeCheckError> {
    if user_id == bot_id {
        return Err(StrikeCheckError::Protected);
    }
    // Fetch current membership and role order immediately before persistence.
    // Old message/interaction role snapshots cannot authorize a new strike.
    let result = async {
        let guild = async { Ok::<_, anyhow::Error>(http.guild(guild_id).await?.model().await?) };
        let bot = async {
            Ok::<_, anyhow::Error>(http.guild_member(guild_id, bot_id).await?.model().await?)
        };
        let target = async {
            Ok::<_, anyhow::Error>(http.guild_member(guild_id, user_id).await?.model().await?)
        };
        let (guild, bot, target) = tokio::join!(guild, bot, target);
        let (guild, bot, target) = (guild?, bot?, target?);
        anyhow::ensure!(
            guild.id == guild_id && bot.user.id == bot_id && target.user.id == user_id,
            "mismatched Discord hierarchy response"
        );
        below_bot(&guild, bot_id, &bot.roles, user_id, &target.roles)
    }
    .await
    .map_err(StrikeCheckError::Unavailable)?;
    if result {
        Ok(())
    } else {
        Err(StrikeCheckError::Protected)
    }
}

fn below_bot(
    guild: &Guild,
    bot_id: Id<UserMarker>,
    bot_roles: &[Id<RoleMarker>],
    user_id: Id<UserMarker>,
    member_roles: &[Id<RoleMarker>],
) -> Result<bool> {
    if user_id == bot_id || user_id == guild.owner_id {
        return Ok(false);
    }
    let bot = highest_role(guild, bot_roles)?;
    let target = highest_role(guild, member_roles)?;
    // Twilight compares position first, then reverse ID for equal positions.
    Ok(bot.id.get() != guild.id.get() && (target.id.get() == guild.id.get() || bot > target))
}

fn highest_role<'a>(guild: &'a Guild, member_roles: &[Id<RoleMarker>]) -> Result<&'a Role> {
    let everyone = guild
        .roles
        .iter()
        .find(|role| role.id.get() == guild.id.get())
        .context("missing @everyone role")?;
    let mut highest = everyone;
    for id in member_roles {
        let role =
            guild.roles.iter().find(|role| role.id == *id).context(
                "a member role is missing from the guild; refusing to assume a lower rank",
            )?;
        // @everyone is always the bottom role, even with unusual role positions.
        if role.id != everyone.id && (highest.id == everyone.id || role > highest) {
            highest = role;
        }
    }
    Ok(highest)
}

#[cfg(test)]
mod tests;
