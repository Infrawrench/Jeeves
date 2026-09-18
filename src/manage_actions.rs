use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::PgPool;
use twilight_http::Client;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{AllowedMentions, Component, Embed, MessageFlags, component::ButtonStyle},
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use twilight_util::builder::{
    embed::{EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder},
    message::{ActionRowBuilder, ButtonBuilder},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    messages: i32,
    strikes: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Page,
    Previous,
    Next,
    Remove { message: bool, id: i32 },
    ConfirmClear,
    Clear,
}

#[derive(Clone, Copy, Debug)]
struct Request {
    guild_id: i64,
    admin_id: i64,
    snapshot: Option<Snapshot>,
    page: i64,
    operation: Operation,
}

impl Request {
    fn parse(interaction: &Interaction, custom_id: Option<&str>) -> Result<Self, &'static str> {
        let guild_id = interaction
            .guild_id
            .ok_or("Use /manageactions in a server.")?;
        let permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .unwrap_or_else(Permissions::empty);
        if !permissions.contains(Permissions::ADMINISTRATOR) {
            return Err("You need the Administrator permission to manage actions.");
        }
        let admin_id = interaction
            .author_id()
            .ok_or("I couldn't identify your account.")?;
        let mut request = Self {
            guild_id: i64::try_from(guild_id.get()).map_err(|_| "Unsupported server ID.")?,
            admin_id: i64::try_from(admin_id.get()).map_err(|_| "Unsupported user ID.")?,
            snapshot: None,
            page: 0,
            operation: Operation::Page,
        };
        let Some(custom_id) = custom_id else {
            if !matches!(
                interaction.data,
                Some(InteractionData::ApplicationCommand(_))
            ) {
                return Err("Invalid manageactions command.");
            }
            return Ok(request);
        };
        let invalid = "This button is invalid. Run /manageactions again.";
        let mut parts = custom_id.split(':');
        if parts.next() != Some("actionadmin") {
            return Err(invalid);
        }
        let operation = parts.next().ok_or(invalid)?;
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
        let [guild, admin, messages, strikes, page, id] = values;
        if guild != request.guild_id || admin != request.admin_id {
            return Err(
                "These controls belong to another admin or server. Run /manageactions yourself.",
            );
        }
        let messages = i32::try_from(messages).map_err(|_| invalid)?;
        let strikes = i32::try_from(strikes).map_err(|_| invalid)?;
        let id = i32::try_from(id).map_err(|_| invalid)?;
        if messages < 0 || strikes < 0 || (messages == 0 && strikes == 0) || page < 0 {
            return Err(invalid);
        }
        request.operation = match (operation, id) {
            ("page", 0) => Operation::Page,
            ("prev", 0) => Operation::Previous,
            ("next", 0) => Operation::Next,
            ("rmmsg", id) if id > 0 && id <= messages => Operation::Remove { message: true, id },
            ("rmstrike", id) if id > 0 && id <= strikes => Operation::Remove { message: false, id },
            ("confirm", 0) => Operation::ConfirmClear,
            ("clear", 0) => Operation::Clear,
            _ => return Err(invalid),
        };
        request.snapshot = Some(Snapshot { messages, strikes });
        request.page = page;
        Ok(request)
    }

    fn button_id(self, operation: Operation, page: i64) -> String {
        let (name, id) = match operation {
            Operation::Page => ("page", 0),
            Operation::Previous => ("prev", 0),
            Operation::Next => ("next", 0),
            Operation::Remove { message: true, id } => ("rmmsg", id),
            Operation::Remove { message: false, id } => ("rmstrike", id),
            Operation::ConfirmClear => ("confirm", 0),
            Operation::Clear => ("clear", 0),
        };
        let snapshot = self.snapshot.unwrap_or(Snapshot {
            messages: 0,
            strikes: 0,
        });
        let values = [
            self.guild_id,
            self.admin_id,
            i64::from(snapshot.messages),
            i64::from(snapshot.strikes),
            page,
            i64::from(id),
        ];
        let bytes: Vec<_> = values.into_iter().flat_map(i64::to_be_bytes).collect();
        format!("actionadmin:{name}:{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

#[derive(sqlx::FromRow)]
struct Action {
    id: i32,
    message: bool,
    question: String,
    code: bool,
    only_channels: Option<Vec<i64>>,
}

impl Action {
    fn kind(&self) -> &'static str {
        match (self.message, self.code) {
            (true, false) => "Message · Interpretation",
            (true, true) => "Message · Code",
            (false, false) => "Strike · Interpretation",
            (false, true) => "Strike · Code",
        }
    }

    fn channels(&self) -> String {
        match &self.only_channels {
            None => "All channels".into(),
            Some(channels) if channels.is_empty() => "No channels (inactive)".into(),
            Some(channels) => {
                let list = channels
                    .iter()
                    .take(10)
                    .map(|id| format!("<#{id}>"))
                    .collect::<Vec<_>>()
                    .join(", ");
                if channels.len() > 10 {
                    format!("{list} and {} more", channels.len() - 10)
                } else {
                    list
                }
            }
        }
    }
}

struct Page {
    request: Request,
    total: i64,
    size: i64,
    actions: Vec<Action>,
}

impl Page {
    fn pages(&self) -> i64 {
        (self.total - 1).max(0) / self.size + 1
    }
}

fn page_size(max_question_bytes: i32) -> i64 {
    // Twilight counts UTF-8 bytes in the aggregate embed limit. Allow room for
    // type labels, ten channel mentions per rule, and the footer.
    5.min(5800 / (i64::from(max_question_bytes) + 360)).max(1)
}

async fn load_page(pool: &PgPool, mut request: Request) -> Result<Page> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let snapshot = match request.snapshot {
        Some(snapshot) => snapshot,
        None => {
            let (messages, strikes) = sqlx::query_as(
                "SELECT (SELECT COALESCE(MAX(id), 0) FROM message_actions WHERE guild_id = $1),
                        (SELECT COALESCE(MAX(id), 0) FROM strike_actions WHERE guild_id = $1)",
            )
            .bind(request.guild_id)
            .fetch_one(&mut *tx)
            .await?;
            Snapshot { messages, strikes }
        }
    };
    request.snapshot = Some(snapshot);
    let (total, max_bytes): (i64, i32) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MAX(octet_length(left(question, 1000))), 0) FROM (
             SELECT question FROM message_actions WHERE guild_id = $1 AND id <= $2
             UNION ALL
             SELECT question FROM strike_actions WHERE guild_id = $1 AND id <= $3
         ) AS actions",
    )
    .bind(request.guild_id)
    .bind(snapshot.messages)
    .bind(snapshot.strikes)
    .fetch_one(&mut *tx)
    .await?;
    let size = page_size(max_bytes);
    request.page = request.page.clamp(0, (total - 1).max(0) / size);
    let actions = sqlx::query_as(
        "SELECT id, message, question, code, only_channels FROM (
             SELECT id, TRUE AS message, question, code IS NOT NULL AS code, only_channels
             FROM message_actions WHERE guild_id = $1 AND id <= $2
             UNION ALL
             SELECT id, FALSE AS message, question, code IS NOT NULL AS code, only_channels
             FROM strike_actions WHERE guild_id = $1 AND id <= $3
         ) AS actions ORDER BY message DESC, id DESC LIMIT $4 OFFSET $5",
    )
    .bind(request.guild_id)
    .bind(snapshot.messages)
    .bind(snapshot.strikes)
    .bind(size)
    .bind(request.page * size)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Page {
        request,
        total,
        size,
        actions,
    })
}

async fn remove(pool: &PgPool, request: Request) -> Result<u64> {
    ensure!(
        matches!(
            request.operation,
            Operation::Remove { .. } | Operation::Clear
        ),
        "not an action removal request"
    );
    let snapshot = request.snapshot.context("missing action cutoffs")?;
    let mut tx = pool.begin().await?;
    // Share the short write lock with /addaction. No model calls run under it.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('actions'), hashtext($1))")
        .bind(request.guild_id.to_string())
        .execute(&mut *tx)
        .await?;
    let mut removed = 0;
    for (message, cutoff, query) in [
        (
            true,
            snapshot.messages,
            "DELETE FROM message_actions WHERE guild_id = $1 AND id <= $2 AND ($3::INTEGER IS NULL OR id = $3)",
        ),
        (
            false,
            snapshot.strikes,
            "DELETE FROM strike_actions WHERE guild_id = $1 AND id <= $2 AND ($3::INTEGER IS NULL OR id = $3)",
        ),
    ] {
        let id = match request.operation {
            Operation::Remove {
                message: selected,
                id,
            } if selected == message => Some(id),
            Operation::Clear => None,
            _ => continue,
        };
        removed += sqlx::query(query)
            .bind(request.guild_id)
            .bind(cutoff)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    tracing::info!(guild_id = request.guild_id, admin_id = request.admin_id, operation = ?request.operation, removed, "Admin removed actions");
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

fn render(page: Page, notice: &str) -> (String, Vec<Embed>, Vec<Component>) {
    let request = page.request;
    let prefix = if notice.is_empty() {
        String::new()
    } else {
        format!("{notice}\n\n")
    };
    if page.actions.is_empty() {
        let empty = if request.snapshot
            == Some(Snapshot {
                messages: 0,
                strikes: 0,
            }) {
            "No actions are configured for this server. Use /addaction to add one."
        } else {
            "No listed actions remain. Run /manageactions again to include any new rules."
        };
        return (format!("{prefix}{empty}"), vec![], vec![]);
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
                request.page,
                "Cancel",
                ButtonStyle::Secondary,
                false,
            ))
            .build();
        return (
            format!(
                "Clear all {} listed actions from this server? Rules added since this view was opened will be kept. This does not undo existing moderation.",
                page.total
            ),
            vec![],
            vec![Component::ActionRow(row)],
        );
    }
    let mut embed = EmbedBuilder::new().title("Server actions");
    let mut removals = ActionRowBuilder::new();
    for (index, action) in page.actions.iter().enumerate() {
        // /addaction limits questions to 1000 characters. Bound legacy SQL-created
        // rules too so a large record cannot break the entire management view.
        let question = if action.question.chars().count() > 1000 {
            format!("{}…", action.question.chars().take(999).collect::<String>())
        } else if action.question.trim().is_empty() {
            "(Empty rule)".into()
        } else {
            action.question.clone()
        };
        embed = embed
            .field(EmbedFieldBuilder::new(
                format!("{}. {}", index + 1, action.kind()),
                question,
            ))
            .field(EmbedFieldBuilder::new("Channels", action.channels()));
        removals = removals.component(button(
            request,
            Operation::Remove {
                message: action.message,
                id: action.id,
            },
            request.page,
            &format!("Remove {}", index + 1),
            ButtonStyle::Danger,
            false,
        ));
    }
    let noun = if page.total == 1 { "action" } else { "actions" };
    let embed = embed
        .footer(EmbedFooterBuilder::new(format!(
            "Page {} of {} · {} {noun}",
            request.page + 1,
            page.pages(),
            page.total
        )))
        .build();
    let navigation = ActionRowBuilder::new()
        .component(button(
            request,
            Operation::Previous,
            (request.page - 1).max(0),
            "Previous",
            ButtonStyle::Secondary,
            request.page == 0,
        ))
        .component(button(
            request,
            Operation::Next,
            (request.page + 1).min(page.pages() - 1),
            "Next",
            ButtonStyle::Secondary,
            request.page + 1 >= page.pages(),
        ))
        .component(button(
            request,
            Operation::ConfirmClear,
            request.page,
            "Clear listed actions",
            ButtonStyle::Secondary,
            false,
        ))
        .build();
    (
        format!("{prefix}Removing actions does not undo existing strikes, bans, or kicks."),
        vec![embed],
        vec![
            Component::ActionRow(navigation),
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
                .context("failed to reject action management request")?;
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
        .context("failed to acknowledge action management request")?;
    let notice = if matches!(
        request.operation,
        Operation::Remove { .. } | Operation::Clear
    ) {
        match remove(pool, request).await {
            Ok(0) => "Those actions were already removed or are no longer in this view.".into(),
            Ok(count) => format!(
                "Removed {count} {}.",
                if count == 1 { "action" } else { "actions" }
            ),
            Err(error) => {
                tracing::error!(interaction_id = %interaction.id, ?error, "Failed to remove actions");
                "I couldn't confirm the removal. Refresh the view and try again.".into()
            }
        }
    } else {
        String::new()
    };
    let (content, embeds, components) = match load_page(pool, request).await {
        Ok(page) => render(page, &notice),
        Err(error) => {
            tracing::error!(interaction_id = %interaction.id, ?error, "Failed to load actions");
            (
                format!("{notice}\nI couldn't load the actions. Run /manageactions again shortly."),
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
        .context("failed to send action management page")?;
    Ok(())
}

#[cfg(test)]
mod tests;
