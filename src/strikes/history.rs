use anyhow::{Context, Result};
use sqlx::PgPool;
use twilight_http::Client;
use twilight_model::{
    application::interaction::Interaction,
    channel::message::{AllowedMentions, Component, Embed, MessageFlags, component::ButtonStyle},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use twilight_util::builder::{
    embed::{EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder},
    message::{ActionRowBuilder, ButtonBuilder},
};

pub(super) const PAGE_SIZE: i64 = 5;

fn page_size(max_reason_bytes: i32) -> i64 {
    // Twilight's aggregate embed validator counts UTF-8 bytes. Reserve room for
    // headers/metadata and reduce page size for long reasons instead of cutting them.
    PAGE_SIZE
        .min(5800 / (i64::from(max_reason_bytes) + 160))
        .max(1)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HistoryRequest {
    pub(super) guild_id: i64,
    pub(super) user_id: i64,
    pub(super) snapshot: Option<i64>,
    pub(super) page: i64,
}

impl HistoryRequest {
    fn parse(interaction: &Interaction, custom_id: Option<&str>) -> Result<Self, &'static str> {
        // Always derive query scope from the authenticated interaction, never a button alone.
        let guild_id = super::snowflake(
            interaction
                .guild_id
                .ok_or("Use /strikes in a server to view your strikes there.")?,
        )?;
        let user_id = super::snowflake(
            interaction
                .author_id()
                .ok_or("I couldn't identify your account.")?,
        )?;
        let mut request = Self {
            guild_id,
            user_id,
            snapshot: None,
            page: 0,
        };
        if let Some(custom_id) = custom_id {
            let invalid = "This page button is invalid. Run /strikes again.";
            let parts: Vec<_> = custom_id.split(':').collect();
            if parts.len() != 6 || parts[0] != "strikes" || !matches!(parts[1], "prev" | "next") {
                return Err(invalid);
            }
            let number = |part: &str| part.parse::<i64>().map_err(|_| invalid);
            if number(parts[2])? != guild_id || number(parts[3])? != user_id {
                return Err(
                    "These buttons belong to someone else's strike history. Run /strikes yourself.",
                );
            }
            let snapshot = number(parts[4])?;
            let page = number(parts[5])?;
            if snapshot <= 0 || page < 0 {
                return Err(invalid);
            }
            request.snapshot = Some(snapshot);
            request.page = page;
        }
        Ok(request)
    }

    fn button_id(self, direction: &str, page: i64) -> String {
        format!(
            "strikes:{direction}:{}:{}:{}:{page}",
            self.guild_id,
            self.user_id,
            self.snapshot.unwrap_or(0)
        )
    }
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct Strike {
    pub(super) id: i64,
    moderator_id: i64,
    reason: String,
    created_at: time::OffsetDateTime,
}

pub(super) struct Page {
    pub(super) request: HistoryRequest,
    pub(super) total: i64,
    pub(super) strikes: Vec<Strike>,
    size: i64,
}

pub(super) async fn load_page(pool: &PgPool, mut request: HistoryRequest) -> Result<Page> {
    let mut tx = pool.begin().await?;
    // Keep the count and selected rows consistent if records change during a request.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let snapshot = match request.snapshot {
        Some(snapshot) => snapshot,
        None => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(MAX(id), 0) FROM strikes WHERE guild_id = $1 AND user_id = $2",
            )
            .bind(request.guild_id)
            .bind(request.user_id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    request.snapshot = Some(snapshot);
    let (total, max_reason_bytes): (i64, i32) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MAX(octet_length(reason)), 0) FROM strikes
         WHERE guild_id = $1 AND user_id = $2 AND id <= $3",
    )
    .bind(request.guild_id)
    .bind(request.user_id)
    .bind(snapshot)
    .fetch_one(&mut *tx)
    .await?;
    let size = page_size(max_reason_bytes);
    request.page = request.page.clamp(0, (total - 1).max(0) / size);
    let strikes = sqlx::query_as::<_, Strike>(
        "SELECT id, moderator_id, reason, created_at FROM strikes
         WHERE guild_id = $1 AND user_id = $2 AND id <= $3
         ORDER BY created_at DESC, id DESC LIMIT $4 OFFSET $5",
    )
    .bind(request.guild_id)
    .bind(request.user_id)
    .bind(snapshot)
    .bind(size)
    .bind(request.page * size)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Page {
        request,
        total,
        strikes,
        size,
    })
}

impl Page {
    pub(super) fn pages(&self) -> i64 {
        (self.total - 1).max(0) / self.size + 1
    }

    pub(super) fn embed(&self, title: &str) -> Option<Embed> {
        if self.strikes.is_empty() {
            return None;
        }
        let mut embed = EmbedBuilder::new().title(title);
        for (index, strike) in self.strikes.iter().enumerate() {
            // These are positions on this page, not database IDs.
            embed = embed
                .field(EmbedFieldBuilder::new(
                    format!("Strike {}", index + 1),
                    strike.reason.clone(),
                ))
                .field(EmbedFieldBuilder::new(
                    "Moderator · Issued",
                    format!(
                        "<@{}> · <t:{}:f>",
                        strike.moderator_id,
                        strike.created_at.unix_timestamp()
                    ),
                ));
        }
        Some(
            embed
                .footer(EmbedFooterBuilder::new(format!(
                    "Page {} of {} · {} {} · Newest first",
                    self.request.page + 1,
                    self.pages(),
                    self.total,
                    if self.total == 1 { "strike" } else { "strikes" },
                )))
                .build(),
        )
    }

    fn render(&self) -> (String, Vec<Embed>, Vec<Component>) {
        let Some(embed) = self.embed("Your strikes") else {
            return ("You have no strikes in this server.".into(), vec![], vec![]);
        };
        let button = |direction, label, page, disabled| {
            Component::Button(
                ButtonBuilder::new(ButtonStyle::Secondary)
                    .custom_id(self.request.button_id(direction, page))
                    .label(label)
                    .disabled(disabled)
                    .build(),
            )
        };
        let row = ActionRowBuilder::new()
            .component(button(
                "prev",
                "Previous",
                (self.request.page - 1).max(0),
                self.request.page == 0,
            ))
            .component(button(
                "next",
                "Next",
                (self.request.page + 1).min(self.pages() - 1),
                self.request.page + 1 >= self.pages(),
            ))
            .build();
        (String::new(), vec![embed], vec![Component::ActionRow(row)])
    }
}

pub async fn handle(
    http: &Client,
    pool: &PgPool,
    interaction: &Interaction,
    custom_id: Option<&str>,
) -> Result<()> {
    let client = http.interaction(interaction.application_id);
    let request = match HistoryRequest::parse(interaction, custom_id) {
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
                .context("failed to reject strike history request")?;
            return Ok(());
        }
    };
    // Button interactions update the existing private message; commands start a new one.
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
        .context("failed to acknowledge strike history request")?;

    let (content, embeds, components) = match load_page(pool, request).await {
        Ok(page) => page.render(),
        Err(error) => {
            tracing::error!(interaction_id = %interaction.id, ?error, "Failed to load strike history");
            (
                "I couldn't load your strikes. Please run /strikes again shortly.".into(),
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
        .context("failed to send strike history page")?;
    Ok(())
}

#[cfg(test)]
mod tests;
