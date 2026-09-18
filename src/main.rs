mod add_action;
mod bot;
mod commands;
mod config;
mod db;
mod gemini;
mod manage_actions;
mod messages;
mod moderation;
mod strikes;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(error) if error.not_found() => {}
        Err(error) => return Err(error).context("failed to load .env"),
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the rustls crypto provider"))?;

    let config = config::Config::from_env()?;
    let gemini = match &config.gemini_backend {
        config::GeminiBackend::Developer { api_key } => {
            gemini::Gemini::new(api_key, &config.gemini_model)?
        }
        config::GeminiBackend::Vertex { project, location } => {
            let gemini = gemini::Gemini::new_vertex(&config.gemini_model, project, location)?;
            tracing::info!(project, location, model = %config.gemini_model, "Using Vertex AI for Gemini");
            gemini
        }
    };
    let jev = jeeves::typesafe::Client::from_env().context("failed to configure Jev client")?;
    let pool = db::connect(config.database, config.database_max_connections).await?;
    let result = bot::run(config.discord_token, pool.clone(), gemini, jev).await;
    pool.close().await;
    result
}
