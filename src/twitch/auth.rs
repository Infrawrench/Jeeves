//! Server-side OAuth code exchange and durable, serialized token refresh.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, StatusCode, header::HeaderValue};
use serde::Deserialize;
use sqlx::PgPool;
use url::Url;

use super::api::{ApiError, Identity};

// Never derive Debug for types containing credentials.
#[derive(Clone)]
pub struct Config {
    pub client_secret: String,
    pub bot_login: String,
    pub public_url: Url,
}

impl Config {
    pub fn new(client_secret: String, bot_login: String, public_url: &str) -> Result<Self> {
        ensure!(
            !client_secret.trim().is_empty(),
            "TWITCH_CLIENT_SECRET is required"
        );
        ensure!(
            !bot_login.is_empty()
                && bot_login.len() <= 25
                && bot_login
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "TWITCH_BOT_LOGIN must be a lowercase Twitch login"
        );
        let public_url = Url::parse(public_url).context("invalid PUBLIC_URL")?;
        ensure!(
            public_url.scheme() == "https"
                && public_url.host_str().is_some()
                && public_url.username().is_empty()
                && public_url.password().is_none()
                && public_url.path() == "/"
                && public_url.query().is_none()
                && public_url.fragment().is_none(),
            "PUBLIC_URL must be an HTTPS origin without a path, credentials, query, or fragment"
        );
        Ok(Self {
            client_secret,
            bot_login,
            public_url,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}auth/twitch/callback", self.public_url)
    }
}

#[derive(sqlx::FromRow)]
pub struct StoredToken {
    pub bot_user_id: String,
    pub access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct Grant {
    access_token: String,
    refresh_token: String,
    token_type: String,
}

#[derive(Debug, thiserror::Error)]
#[error("Twitch bot authorization is required; use the hosted setup page")]
pub struct AuthorizationRequired;

pub struct Manager {
    pub config: Config,
    pub client_id: String,
    pub pool: PgPool,
    http: Client,
    token_url: String,
    validate_url: String,
}

impl Manager {
    pub fn new(config: Config, client_id: String, pool: PgPool) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            config,
            client_id,
            pool,
            http: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token_url: "https://id.twitch.tv/oauth2/token".into(),
            validate_url: "https://id.twitch.tv/oauth2/validate".into(),
        }))
    }

    pub async fn load(&self) -> Result<Option<StoredToken>> {
        Ok(sqlx::query_as("SELECT bot_user_id, access_token, refresh_token FROM twitch_oauth_tokens WHERE client_id = $1 AND bot_login = $2")
            .bind(&self.client_id).bind(&self.config.bot_login).fetch_optional(&self.pool).await?)
    }

    pub async fn access_token(&self) -> Result<String> {
        Ok(self
            .load()
            .await?
            .ok_or(AuthorizationRequired)?
            .access_token)
    }

    async fn grant(&self, parameters: &[(&str, &str)]) -> Result<Grant> {
        let mut form = parameters.to_vec();
        form.extend([
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.config.client_secret.as_str()),
        ]);
        let response = self.http.post(&self.token_url).form(&form).send().await?;
        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
        ) {
            return Err(AuthorizationRequired.into());
        }
        if !response.status().is_success() {
            return Err(ApiError(response.status()).into());
        }
        // Never include an upstream response body in errors or logs.
        let grant: Grant = response
            .json()
            .await
            .context("invalid Twitch token response")?;
        ensure!(
            !grant.access_token.is_empty()
                && !grant.refresh_token.is_empty()
                && grant.token_type.eq_ignore_ascii_case("bearer"),
            "invalid Twitch token grant"
        );
        Ok(grant)
    }

    async fn identity(&self, access_token: &str, expected_user: Option<&str>) -> Result<Identity> {
        let response = self
            .http
            .get(&self.validate_url)
            .header("Authorization", bearer(access_token)?)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError(response.status()).into());
        }
        let identity: Identity = response.json().await.context("invalid Twitch identity")?;
        identity.check(&self.client_id, expected_user)?;
        ensure!(
            identity.login == self.config.bot_login,
            "Authorize the configured bot account, not a broadcaster's account"
        );
        Ok(identity)
    }

    pub async fn exchange_code(&self, code: &str) -> Result<()> {
        let grant = self
            .grant(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.config.redirect_uri()),
            ])
            .await?;
        let identity = self.identity(&grant.access_token, None).await?;
        // The row lock taken by this upsert also serializes reauthorization against refresh.
        sqlx::query("INSERT INTO twitch_oauth_tokens (client_id, bot_user_id, bot_login, access_token, refresh_token) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (client_id) DO UPDATE SET bot_user_id = EXCLUDED.bot_user_id, bot_login = EXCLUDED.bot_login, access_token = EXCLUDED.access_token, refresh_token = EXCLUDED.refresh_token, updated_at = now()")
            .bind(&self.client_id).bind(identity.user_id).bind(&self.config.bot_login)
            .bind(grant.access_token).bind(grant.refresh_token).execute(&self.pool).await?;
        Ok(())
    }

    /// Only refresh after a 401; concurrent failures reuse the first caller's new token.
    /// The database lock also protects rotating refresh tokens across process restarts.
    pub async fn refresh_after(&self, rejected: &str) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let stored: StoredToken = sqlx::query_as("SELECT bot_user_id, access_token, refresh_token FROM twitch_oauth_tokens WHERE client_id = $1 AND bot_login = $2 FOR UPDATE")
            .bind(&self.client_id).bind(&self.config.bot_login).fetch_optional(&mut *tx).await?.ok_or(AuthorizationRequired)?;
        if stored.access_token != rejected {
            tx.commit().await?;
            return Ok(stored.access_token);
        }
        let grant = self
            .grant(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &stored.refresh_token),
            ])
            .await?;
        self.identity(&grant.access_token, Some(&stored.bot_user_id))
            .await?;
        sqlx::query("UPDATE twitch_oauth_tokens SET access_token = $2, refresh_token = $3, updated_at = now() WHERE client_id = $1")
            .bind(&self.client_id).bind(&grant.access_token).bind(grant.refresh_token).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(grant.access_token)
    }
}

pub fn bearer(token: &str) -> Result<HeaderValue> {
    let mut header =
        HeaderValue::from_str(&format!("Bearer {token}")).context("invalid Twitch credential")?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod tests;
