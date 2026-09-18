use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::PgPool;
use twilight_http::Client;
use twilight_model::{
    application::{
        command::CommandOption,
        interaction::{Interaction, InteractionData, application_command::CommandOptionValue},
    },
    channel::message::{AllowedMentions, Component, Embed, MessageFlags, component::ButtonStyle},
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use twilight_util::builder::{
    command::UserBuilder,
    message::{ActionRowBuilder, ButtonBuilder},
};

use super::history::{self, HistoryRequest, Page};

pub fn options() -> Vec<CommandOption> {
    vec![
        UserBuilder::new("user", "User whose strikes you want to view or remove")
            .required(true)
            .build(),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Page,
    Previous,
    Next,
    Remove(i64),
    ConfirmClear,
    Clear,
}

#[derive(Clone, Copy, Debug)]
struct Request {
    history: HistoryRequest,
    admin_id: i64,
    operation: Operation,
}

impl Request {
    fn parse(interaction: &Interaction, custom_id: Option<&str>) -> Result<Self, &'static str> {
        let guild_id = super::snowflake(
            interaction
                .guild_id
                .ok_or("Use /managestrikes in a server.")?,
        )?;
        let permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .unwrap_or_else(Permissions::empty);
        if !permissions.contains(Permissions::ADMINISTRATOR) {
            return Err("You need the Administrator permission to manage strikes.");
        }
        let admin_id = super::snowflake(
            interaction
                .author_id()
                .ok_or("I couldn't identify your account.")?,
        )?;
        if let Some(custom_id) = custom_id {
            let invalid = "This button is invalid. Run /managestrikes again.";
            let mut parts = custom_id.split(':');
            if parts.next() != Some("strikeadmin") {
                return Err(invalid);
            }
            let action = parts.next().ok_or(invalid)?;
            let bytes = URL_SAFE_NO_PAD
                .decode(parts.next().ok_or(invalid)?)
                .map_err(|_| invalid)?;
            if parts.next().is_some() || bytes.len() != 48 {
                return Err(invalid);
            }
            let mut values = [0_i64; 6];
            for (value, chunk) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
                *value = i64::from_be_bytes(*chunk);
            }
            let [
                button_guild,
                button_admin,
                user_id,
                snapshot,
                page,
                strike_id,
            ] = values;
            if button_guild != guild_id || button_admin != admin_id {
                return Err(
                    "These controls belong to another admin or server. Run /managestrikes yourself.",
                );
            }
            if user_id <= 0 || snapshot <= 0 || page < 0 {
                return Err(invalid);
            }
            let operation = match (action, strike_id) {
                ("page", 0) => Operation::Page,
                ("prev", 0) => Operation::Previous,
                ("next", 0) => Operation::Next,
                ("remove", id) if id > 0 && id <= snapshot => Operation::Remove(id),
                ("confirm", 0) => Operation::ConfirmClear,
                ("clear", 0) => Operation::Clear,
                _ => return Err(invalid),
            };
            return Ok(Self {
                history: HistoryRequest {
                    guild_id,
                    user_id,
                    snapshot: Some(snapshot),
                    page,
                },
                admin_id,
                operation,
            });
        }
        let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_ref() else {
            return Err("Invalid managestrikes command.");
        };
        let user_id = command
            .options
            .iter()
            .find_map(|option| match option.value {
                CommandOptionValue::User(id) if option.name == "user" => Some(id),
                _ => None,
            })
            .ok_or("Choose a user whose strikes you want to manage.")?;
        // Former/banned members retain server-scoped history and can be managed too.
        Ok(Self {
            history: HistoryRequest {
                guild_id,
                user_id: super::snowflake(user_id)?,
                snapshot: None,
                page: 0,
            },
            admin_id,
            operation: Operation::Page,
        })
    }

    fn button_id(self, operation: Operation, page: i64) -> String {
        let (action, strike_id) = match operation {
            Operation::Page => ("page", 0),
            Operation::Previous => ("prev", 0),
            Operation::Next => ("next", 0),
            Operation::Remove(id) => ("remove", id),
            Operation::ConfirmClear => ("confirm", 0),
            Operation::Clear => ("clear", 0),
        };
        // Six full-width IDs/counters fit Discord's 100-character custom ID limit.
        let values = [
            self.history.guild_id,
            self.admin_id,
            self.history.user_id,
            self.history.snapshot.unwrap_or(0),
            page,
            strike_id,
        ];
        let bytes: Vec<_> = values.into_iter().flat_map(i64::to_be_bytes).collect();
        format!("strikeadmin:{action}:{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

async fn remove(pool: &PgPool, request: Request) -> Result<u64> {
    let strike_id = match request.operation {
        Operation::Remove(id) => Some(id),
        Operation::Clear => None,
        _ => anyhow::bail!("not a strike removal request"),
    };
    let snapshot = request.history.snapshot.context("missing strike cutoff")?;
    let mut tx = pool.begin().await?;
    // Use the same member lock as issuance; a clear only affects the displayed cutoff.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(request.history.guild_id.to_string())
        .bind(request.history.user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query(
        "DELETE FROM strikes WHERE guild_id = $1 AND user_id = $2 AND id <= $3
         AND ($4::BIGINT IS NULL OR id = $4)",
    )
    .bind(request.history.guild_id)
    .bind(request.history.user_id)
    .bind(snapshot)
    .bind(strike_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    tracing::info!(
        guild_id = request.history.guild_id,
        user_id = request.history.user_id,
        admin_id = request.admin_id,
        ?strike_id,
        through_strike_id = snapshot,
        removed,
        "Admin removed strikes"
    );
    Ok(removed)
}

fn button(
    request: Request,
    operation: Operation,
    page: i64,
    label: &str,
    style: ButtonStyle,
    disabled: bool,
) -> Component {
    Component::Button(
        ButtonBuilder::new(style)
            .custom_id(request.button_id(operation, page))
            .label(label)
            .disabled(disabled)
            .build(),
    )
}

fn render(page: Page, mut request: Request, notice: &str) -> (String, Vec<Embed>, Vec<Component>) {
    request.history = page.request;
    let user_id = request.history.user_id;
    let prefix = if notice.is_empty() {
        String::new()
    } else {
        format!("{notice}\n\n")
    };
    if page.strikes.is_empty() {
        let message = if request.history.snapshot == Some(0) {
            format!("<@{user_id}> has no strikes in this server.")
        } else {
            format!(
                "No listed strikes remain for <@{user_id}>. Run /managestrikes again to include any new strikes."
            )
        };
        return (format!("{prefix}{message}"), vec![], vec![]);
    }
    if request.operation == Operation::ConfirmClear {
        let row = ActionRowBuilder::new()
            .component(button(
                request,
                Operation::Clear,
                0,
                "Confirm clear",
                ButtonStyle::Danger,
                false,
            ))
            .component(button(
                request,
                Operation::Page,
                request.history.page,
                "Cancel",
                ButtonStyle::Secondary,
                false,
            ))
            .build();
        return (
            format!(
                "Clear all {} listed strikes for <@{user_id}> in this server? Strikes added since this view was opened will be kept. This does not undo bans or kicks.",
                page.total
            ),
            vec![],
            vec![Component::ActionRow(row)],
        );
    }
    let row = ActionRowBuilder::new()
        .component(button(
            request,
            Operation::Previous,
            (request.history.page - 1).max(0),
            "Previous",
            ButtonStyle::Secondary,
            request.history.page == 0,
        ))
        .component(button(
            request,
            Operation::Next,
            (request.history.page + 1).min(page.pages() - 1),
            "Next",
            ButtonStyle::Secondary,
            request.history.page + 1 >= page.pages(),
        ))
        .component(button(
            request,
            Operation::ConfirmClear,
            request.history.page,
            "Clear listed strikes",
            ButtonStyle::Secondary,
            false,
        ))
        .build();
    let mut removals = ActionRowBuilder::new();
    for (index, strike) in page.strikes.iter().enumerate() {
        removals = removals.component(button(
            request,
            Operation::Remove(strike.id),
            request.history.page,
            &format!("Remove {}", index + 1),
            ButtonStyle::Danger,
            false,
        ));
    }
    (
        format!("{prefix}Strikes for <@{user_id}>. Removing strikes does not undo bans or kicks."),
        vec![page.embed("Manage strikes").expect("page contains strikes")],
        vec![
            Component::ActionRow(row),
            Component::ActionRow(removals.build()),
        ],
    )
}

pub async fn handle(
    http: &Client,
    pool: &PgPool,
    interaction: &Interaction,
    custom_id: Option<&str>,
) -> Result<()> {
    let client = http.interaction(interaction.application_id);
    let request = match Request::parse(interaction, custom_id) {
        Ok(request) => request,
        Err(message) => {
            client
                .create_response(
                    interaction.id,
                    &interaction.token,
                    &InteractionResponse {
                        kind: InteractionResponseType::ChannelMessageWithSource,
                        data: Some(InteractionResponseData {
                            content: Some(message.into()),
                            flags: Some(MessageFlags::EPHEMERAL),
                            allowed_mentions: Some(AllowedMentions::default()),
                            ..Default::default()
                        }),
                    },
                )
                .await
                .context("failed to reject strike management request")?;
            return Ok(());
        }
    };
    client
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: if custom_id.is_some() {
                    InteractionResponseType::DeferredUpdateMessage
                } else {
                    InteractionResponseType::DeferredChannelMessageWithSource
                },
                data: custom_id.is_none().then(|| InteractionResponseData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            },
        )
        .await
        .context("failed to acknowledge strike management request")?;

    let mut notice = String::new();
    if matches!(request.operation, Operation::Remove(_) | Operation::Clear) {
        notice = match remove(pool, request).await {
            Ok(0) => "Those strikes were already removed or are no longer in this view.".into(),
            Ok(count) => {
                let noun = if count == 1 { "strike" } else { "strikes" };
                format!("Removed {count} {noun} for <@{}>.", request.history.user_id)
            }
            Err(error) => {
                tracing::error!(interaction_id = %interaction.id, ?error, "Failed to remove strikes");
                "I couldn't confirm the removal. Refresh the view and try again.".into()
            }
        };
    }
    let (content, embeds, components) = match history::load_page(pool, request.history).await {
        Ok(page) => render(page, request, &notice),
        Err(error) => {
            tracing::error!(interaction_id = %interaction.id, ?error, "Failed to load admin strike history");
            (
                format!("{notice}\nI couldn't load the strikes. Run /managestrikes again shortly."),
                vec![],
                vec![],
            )
        }
    };
    client
        .update_response(&interaction.token)
        .content(Some(&content))
        .embeds(Some(&embeds))
        .components(Some(&components))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await
        .context("failed to send strike management page")?;
    Ok(())
}

#[cfg(test)]
mod tests;
