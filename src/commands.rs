use anyhow::{Context, Result};
use sqlx::PgPool;
use twilight_http::Client;
use twilight_model::{
    application::interaction::{Interaction, InteractionData, InteractionType},
    channel::message::{AllowedMentions, MessageFlags},
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{Id, marker::ApplicationMarker},
};
use twilight_util::builder::command::{StringBuilder, UserBuilder};

const PING_DESCRIPTION: &str = "Check the bot and its PostgreSQL connection";

pub async fn register(http: &Client, application_id: Id<ApplicationMarker>) -> Result<()> {
    let client = http.interaction(application_id);
    client
        .create_global_command()
        .chat_input("ping", PING_DESCRIPTION)
        .await?;
    tracing::info!("Registered /ping globally");
    client
        .create_global_command()
        .chat_input("invite", "Get a link to add Jeeves to your server")
        .await?;
    tracing::info!("Registered /invite globally");
    let options = [
        UserBuilder::new("user", "Server member to strike")
            .required(true)
            .build(),
        StringBuilder::new("reason", "Reason for the strike")
            .required(true)
            .min_length(1)
            .max_length(crate::strikes::MAX_REASON_LENGTH)
            .build(),
    ];
    client
        .create_global_command()
        .chat_input(
            "strike",
            "Record a moderation strike against a server member",
        )
        .command_options(&options)
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .dm_permission(false)
        .await?;
    tracing::info!("Registered /strike globally");
    client
        .create_global_command()
        .chat_input("strikes", "Privately view your strikes in this server")
        .dm_permission(false)
        .await?;
    tracing::info!("Registered /strikes globally");
    client
        .create_global_command()
        .chat_input(
            "managestrikes",
            "View and remove a user's strikes in this server",
        )
        .command_options(&crate::strikes::manage::options())
        .default_member_permissions(Permissions::ADMINISTRATOR)
        .dm_permission(false)
        .await?;
    tracing::info!("Registered /managestrikes globally");
    client
        .create_global_command()
        .chat_input(
            "addaction",
            "Add an automatic moderation rule to this server",
        )
        .command_options(&crate::add_action::options())
        .default_member_permissions(Permissions::ADMINISTRATOR)
        .dm_permission(false)
        .await?;
    tracing::info!("Registered /addaction globally");
    client
        .create_global_command()
        .chat_input(
            "manageactions",
            "View and remove this server's moderation rules",
        )
        .default_member_permissions(Permissions::ADMINISTRATOR)
        .dm_permission(false)
        .await?;
    tracing::info!("Registered /manageactions globally");
    Ok(())
}

pub async fn handle(
    http: &Client,
    pool: &PgPool,
    moderation: &crate::moderation::Moderation,
    gemini: &crate::gemini::Gemini,
    jev: &jeeves::typesafe::Client,
    interaction: Interaction,
) -> Result<()> {
    if interaction.kind == InteractionType::MessageComponent {
        if let Some(InteractionData::MessageComponent(component)) = interaction.data.as_ref()
            && component.custom_id.starts_with("actionadmin:")
        {
            return crate::manage_actions::handle(
                http,
                pool,
                &interaction,
                Some(&component.custom_id),
            )
            .await;
        }
        if let Some(InteractionData::MessageComponent(component)) = interaction.data.as_ref()
            && component.custom_id.starts_with("strikeadmin:")
        {
            return crate::strikes::manage::handle(
                http,
                pool,
                &interaction,
                Some(&component.custom_id),
            )
            .await;
        }
        if let Some(InteractionData::MessageComponent(component)) = interaction.data.as_ref()
            && component.custom_id.starts_with("strikes:")
        {
            return crate::strikes::history::handle(
                http,
                pool,
                &interaction,
                Some(&component.custom_id),
            )
            .await;
        }
        return Ok(());
    }
    if interaction.kind != InteractionType::ApplicationCommand {
        return Ok(());
    }
    let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_ref() else {
        return Ok(());
    };
    if command.name == "strikes" {
        return crate::strikes::history::handle(http, pool, &interaction, None).await;
    }
    if command.name == "managestrikes" {
        return crate::strikes::manage::handle(http, pool, &interaction, None).await;
    }
    if command.name == "manageactions" {
        return crate::manage_actions::handle(http, pool, &interaction, None).await;
    }

    let client = http.interaction(interaction.application_id);
    // Acknowledge before querying PostgreSQL to meet Discord's response deadline.
    client
        .create_response(
            interaction.id,
            &interaction.token,
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: matches!(command.name.as_str(), "invite" | "strike" | "addaction").then(
                    || InteractionResponseData {
                        flags: Some(MessageFlags::EPHEMERAL),
                        ..Default::default()
                    },
                ),
            },
        )
        .await
        .context("failed to acknowledge interaction")?;

    let content = match command.name.as_str() {
        "invite" => format!(
            "[Invite Jeeves to your server]({})",
            invite_url(interaction.application_id)
        ),
        "strike" => crate::strikes::handle(moderation, &interaction).await,
        "addaction" => crate::add_action::handle(http, pool, gemini, jev, &interaction).await,
        _ => "This command is not supported yet.".into(),
    };

    client
        .update_response(&interaction.token)
        .content(Some(&content))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await
        .context("failed to send interaction response")?;
    Ok(())
}

fn invite_url(application_id: Id<ApplicationMarker>) -> String {
    let permissions = Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::MANAGE_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::KICK_MEMBERS
        | Permissions::BAN_MEMBERS;

    format!(
        "https://discord.com/oauth2/authorize?client_id={application_id}&scope=bot%20applications.commands&permissions={}&integration_type=0",
        permissions.bits()
    )
}
