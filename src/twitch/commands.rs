use anyhow::Result;
use jeeves::typesafe;
use sqlx::PgPool;

use super::{Action, ChatMessage, add_action, api::Api, store};
use crate::gemini::{self, Gemini};

const HELP: &str = "!addaction <rule in plain English> (also !jeeves addaction); !jeeves rules [after-id]; !jeeves remove <rule-id>; !jeeves strikes [after-id]; !jeeves forgive <strike-id>. Example: !addaction Ban users after three strikes. Rule management and forgiveness require moderator status. To stop Jeeves, send !leave in the bot account's chat.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    AddAction(String),
    Add { action: Action, condition: String },
    Escalate { count: i32, action: Action },
    Rules(i64),
    Remove(i64),
    Strikes(i64),
    Forgive(i64),
    Help,
}

pub fn parse(text: &str) -> Option<Result<Command, &'static str>> {
    let (prefix, rest) = word(text.trim());
    if prefix == "!addaction" {
        return Some(plain_rule(rest));
    }
    if prefix != "!jeeves" {
        return None;
    }
    Some(parse_args(rest))
}

fn parse_args(rest: &str) -> Result<Command, &'static str> {
    let (verb, rest) = word(rest);
    match verb {
        "addaction" => plain_rule(rest),
        "add" => {
            let (action, condition) = action(rest)?;
            if condition.is_empty() || condition.chars().count() > 1000 {
                return Err("Provide a condition of 1–1,000 characters.");
            }
            Ok(Command::Add {
                action,
                condition: condition.into(),
            })
        }
        "escalate" => {
            let (count, rest) = word(rest);
            let count = count
                .parse::<i32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("Provide a positive strike threshold.")?;
            let (action, extra) = action(rest)?;
            if !extra.is_empty() || !matches!(action, Action::Ban | Action::Timeout(_)) {
                return Err(
                    "Use !jeeves escalate <strikes> ban or !jeeves escalate <strikes> timeout <seconds>.",
                );
            }
            Ok(Command::Escalate { count, action })
        }
        "rules" => Ok(Command::Rules(cursor(rest)?)),
        "strikes" => Ok(Command::Strikes(cursor(rest)?)),
        "remove" => Ok(Command::Remove(id(rest)?)),
        "forgive" => Ok(Command::Forgive(id(rest)?)),
        "help" | "" if rest.is_empty() => Ok(Command::Help),
        _ => Err(HELP),
    }
}

fn plain_rule(rule: &str) -> Result<Command, &'static str> {
    if rule.is_empty() || rule.chars().count() > 1000 {
        return Err(
            "Use !addaction followed by a rule of 1–1,000 characters, such as: Strike users for spam.",
        );
    }
    Ok(Command::AddAction(rule.to_owned()))
}

fn word(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map(|(first, rest)| (first, rest.trim()))
        .unwrap_or((text, ""))
}

fn id(text: &str) -> Result<i64, &'static str> {
    text.parse()
        .ok()
        .filter(|value| *value > 0)
        .ok_or("Provide a positive numeric ID.")
}

fn cursor(text: &str) -> Result<i64, &'static str> {
    if text.is_empty() { Ok(0) } else { id(text) }
}

fn action(text: &str) -> Result<(Action, &str), &'static str> {
    let (name, rest) = word(text);
    match name {
        "strike" => Ok((Action::Strike, rest)),
        "delete" => Ok((Action::Delete, rest)),
        "ban" => Ok((Action::Ban, rest)),
        "timeout" => {
            let (seconds, condition) = word(rest);
            let seconds = seconds
                .parse()
                .ok()
                .filter(|n| (1..=1_209_600).contains(n))
                .ok_or("Timeouts must be 1–1,209,600 seconds.")?;
            Ok((Action::Timeout(seconds), condition))
        }
        _ => Err("Choose strike, delete, ban, or timeout <seconds>, followed by the condition."),
    }
}

impl Command {
    pub fn requires_moderator(&self) -> bool {
        !matches!(self, Self::Help | Self::Strikes(_))
    }
}

pub async fn handle(
    pool: &PgPool,
    api: &Api,
    jev: &typesafe::Client,
    gemini: &Gemini,
    message: &ChatMessage,
    command: Result<Command, &'static str>,
) -> Result<()> {
    let command = match command {
        Ok(command) => command,
        Err(error) => return api.say(&message.broadcaster_user_id, error).await,
    };
    if command.requires_moderator() && !message.is_moderator() {
        return api
            .say(
                &message.broadcaster_user_id,
                "Only this channel's broadcaster and moderators can manage Jeeves.",
            )
            .await;
    }
    let channel = &message.broadcaster_user_id;
    let response = match command {
        Command::AddAction(rule) => {
            let action = match add_action::prepare(jev, gemini, &rule).await {
                Ok(action) => action,
                Err(error) => {
                    tracing::warn!(?error, "Twitch rule creation failed");
                    let response = if let Some(error) =
                        error.downcast_ref::<add_action::RuleError>()
                    {
                        error.0
                    } else if let Some(error) = error.downcast_ref::<gemini::ApiError>() {
                        error.user_message()
                    } else {
                        "I couldn't interpret that rule right now. Please try again. No action was saved."
                    };
                    return api.say(channel, response).await;
                }
            };
            let id = store::add_rule(
                pool,
                message,
                &action.condition,
                &action.action,
                action.strike_threshold,
            )
            .await?;
            if let Some(count) = action.strike_threshold {
                format!(
                    "Saved rule {id}: {} on a new strike when the member has at least {count} active strikes.",
                    action.action.label()
                )
            } else {
                format!(
                    "Saved rule {id}: {} when {}",
                    action.action.label(),
                    action.condition
                )
            }
        }
        Command::Add { action, condition } => {
            let id = store::add_rule(pool, message, &condition, &action, None).await?;
            format!("Saved rule {id}: {} when {condition}", action.label())
        }
        Command::Escalate { count, action } => {
            let condition = format!("At least {count} active strikes");
            let id = store::add_rule(pool, message, &condition, &action, Some(count)).await?;
            format!(
                "Saved rule {id}: {} on a new strike when the member has at least {count} active strikes.",
                action.label()
            )
        }
        Command::Rules(after) => {
            let rules = store::rules(pool, channel).await?;
            let entries = rules
                .into_iter()
                .filter(|rule| rule.id > after)
                .map(|rule| {
                    Ok((
                        rule.id,
                        format!("{}: {}", rule.outcome()?.label(), rule.condition),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            page("rules", &entries)
        }
        Command::Remove(id) => {
            let removed = sqlx::query("DELETE FROM twitch_rules WHERE channel_id = $1 AND id = $2")
                .bind(channel)
                .bind(id)
                .execute(pool)
                .await?
                .rows_affected();
            if removed > 0 {
                format!("Removed rule {id}.")
            } else {
                "Rule not found in this channel.".into()
            }
        }
        Command::Strikes(after) => {
            let entries = store::strikes(pool, channel, &message.chatter_user_id, after).await?;
            format!(
                "@{} {}",
                message.chatter_user_login,
                page("strikes", &entries)
            )
        }
        Command::Forgive(id) => {
            if store::remove_strike(pool, channel, id).await? {
                format!("Removed strike {id}. Existing timeouts and bans are unchanged.")
            } else {
                "Active strike not found in this channel.".into()
            }
        }
        Command::Help => HELP.into(),
    };
    api.say(channel, &response).await
}

fn page(kind: &str, entries: &[(i64, String)]) -> String {
    if entries.is_empty() {
        return format!("No more {kind}.");
    }
    // One entry per response leaves room for Unicode and a working next-page cursor.
    let (id, description) = &entries[0];
    let description: String = description.chars().take(300).collect();
    let next = if entries.len() > 1 {
        format!(" Next: !jeeves {kind} {id}")
    } else {
        String::new()
    };
    format!("{kind} #{id}: {description}{next}")
}
