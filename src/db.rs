use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

pub async fn connect(options: PgConnectOptions, max_connections: u32) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .context("failed to connect to PostgreSQL")?;

    sqlx::migrate!()
        .run(&pool)
        .await
        .context("failed to run database migrations")?;
    tracing::info!("PostgreSQL connected and migrations applied");
    Ok(pool)
}
