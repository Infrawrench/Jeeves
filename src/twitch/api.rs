use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method, StatusCode, header::HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Action, ChatMessage, Config, auth};

pub const SCOPES: [&str; 5] = [
    "user:read:chat",
    "user:write:chat",
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
    "user:read:moderated_channels",
];

#[derive(Clone)]
pub struct Api {
    http: Client,
    client_id: String,
    token: HeaderValue,
    auth: Option<Arc<auth::Manager>>,
    base: String,
    pub user_id: String,
    next_chat: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}

#[derive(Deserialize)]
pub(super) struct Identity {
    pub client_id: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub login: String,
}

impl Identity {
    pub fn check(&self, client_id: &str, user_id: Option<&str>) -> Result<()> {
        ensure!(
            self.client_id == client_id,
            "TWITCH_CLIENT_ID does not match the access token"
        );
        ensure!(
            !self.user_id.is_empty(),
            "Twitch requires a user access token, not an app token"
        );
        ensure!(
            user_id.is_none_or(|id| id == self.user_id),
            "Twitch token user changed"
        );
        for scope in SCOPES {
            ensure!(
                self.scopes.iter().any(|value| value == scope),
                "Twitch token is missing scope {scope}"
            );
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Twitch API returned HTTP {0}")]
pub struct ApiError(pub StatusCode);

impl Api {
    #[cfg(test)]
    pub(super) fn for_test_managed(base: String, auth: Arc<auth::Manager>) -> Self {
        let mut api = Self::for_test(base);
        api.client_id = auth.client_id.clone();
        api.auth = Some(auth);
        api
    }

    #[cfg(test)]
    pub(super) fn for_test(base: String) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            client_id: "client".into(),
            token: HeaderValue::from_static("Bearer test-token"),
            auth: None,
            base,
            user_id: "300".into(),
            next_chat: Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        }
    }

    pub async fn new(config: &Config, auth: Option<Arc<auth::Manager>>) -> Result<Self> {
        let token = match &auth {
            Some(auth) => auth::bearer(&auth.access_token().await?)?,
            None => auth::bearer(
                config
                    .access_token
                    .as_deref()
                    .context("TWITCH_ACCESS_TOKEN is required")?,
            )?,
        };
        let mut api = Self {
            http: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            client_id: config.client_id.clone(),
            token,
            auth,
            base: "https://api.twitch.tv/helix".into(),
            user_id: String::new(),
            next_chat: Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        };
        let identity = api.validate().await?;
        if let Some(auth) = &api.auth {
            ensure!(
                identity.login == auth.config.bot_login,
                "Twitch token does not belong to the configured bot account"
            );
        }
        api.user_id = identity.user_id;
        Ok(api)
    }

    pub async fn validate(&self) -> Result<Identity> {
        let mut token = self.authorization().await?;
        let mut response = self
            .http
            .get("https://id.twitch.tv/oauth2/validate")
            .header("Authorization", token.clone())
            .send()
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED && self.auth.is_some() {
            token = self.refreshed(&token).await?;
            response = self
                .http
                .get("https://id.twitch.tv/oauth2/validate")
                .header("Authorization", token)
                .send()
                .await?;
        }
        let identity: Identity = checked(response)
            .await?
            .json()
            .await
            .context("invalid Twitch token validation response")?;
        identity.check(
            &self.client_id,
            (!self.user_id.is_empty()).then_some(self.user_id.as_str()),
        )?;
        Ok(identity)
    }

    async fn authorization(&self) -> Result<HeaderValue> {
        match &self.auth {
            Some(auth) => auth::bearer(&auth.access_token().await?),
            None => Ok(self.token.clone()),
        }
    }

    async fn refreshed(&self, rejected: &HeaderValue) -> Result<HeaderValue> {
        let token = rejected
            .to_str()?
            .strip_prefix("Bearer ")
            .context("invalid Twitch authorization header")?;
        auth::bearer(
            &self
                .auth
                .as_ref()
                .context("Twitch OAuth is not configured")?
                .refresh_after(token)
                .await?,
        )
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("Client-Id", &self.client_id)
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let token = self.authorization().await?;
        let response = request
            .try_clone()
            .context("Twitch request cannot be retried")?
            .header("Authorization", token.clone())
            .send()
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED && self.auth.is_some() {
            // A rejected request has no moderation effect. Never retry timeouts or 5xx responses.
            let token = self.refreshed(&token).await?;
            return checked(request.header("Authorization", token).send().await?).await;
        }
        checked(response).await
    }

    pub async fn user_id_for_login(&self, login: &str) -> Result<Option<String>> {
        let response: Value = self
            .request(Method::GET, "/users", &[("login", login)], None)
            .await?
            .json()
            .await?;
        response["data"]
            .as_array()
            .context("missing Twitch users")?
            .first()
            .map(|user| {
                user["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .context("missing Twitch user ID")
            })
            .transpose()
    }

    pub async fn moderates(&self, channel: &str) -> Result<bool> {
        let mut after = String::new();
        loop {
            let mut query = vec![("user_id", self.user_id.as_str()), ("first", "100")];
            if !after.is_empty() {
                query.push(("after", &after));
            }
            let data: Value = self
                .request(Method::GET, "/moderation/channels", &query, None)
                .await?
                .json()
                .await?;
            let channels = data["data"]
                .as_array()
                .context("missing moderated channels")?;
            if channels
                .iter()
                .any(|item| item["broadcaster_id"].as_str() == Some(channel))
            {
                return Ok(true);
            }
            match data["pagination"]["cursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
            {
                Some(cursor) => {
                    ensure!(cursor != after, "repeated Twitch pagination cursor");
                    after = cursor.to_owned();
                }
                None => return Ok(false),
            }
        }
    }

    pub async fn subscribe(&self, session: &str, channel: &str, kind: &str) -> Result<String> {
        let response: Value = self
            .request(
                Method::POST,
                "/eventsub/subscriptions",
                &[],
                Some(json!({
                    "type": kind, "version": "1",
                    "condition": {"broadcaster_user_id": channel, "user_id": self.user_id},
                    "transport": {"method": "websocket", "session_id": session},
                })),
            )
            .await?
            .json()
            .await?;
        response["data"][0]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .context("missing Twitch subscription ID")
    }

    pub async fn unsubscribe(&self, id: &str) -> Result<()> {
        match self
            .request(
                Method::DELETE,
                "/eventsub/subscriptions",
                &[("id", id)],
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if error
                    .downcast_ref::<ApiError>()
                    .is_some_and(|error| error.0 == StatusCode::NOT_FOUND) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn say(&self, channel: &str, message: &str) -> Result<()> {
        {
            // Stay below the moderator chat allowance across all channel workers.
            let mut next = self.next_chat.lock().await;
            tokio::time::sleep_until(*next).await;
            *next = tokio::time::Instant::now() + Duration::from_millis(350);
        }
        let text: String = message.chars().take(500).collect();
        let response: Value = self
            .request(
                Method::POST,
                "/chat/messages",
                &[],
                Some(json!({
                    "broadcaster_id": channel, "sender_id": self.user_id, "message": text,
                })),
            )
            .await?
            .json()
            .await?;
        ensure!(
            response["data"][0]["is_sent"] == true,
            "Twitch did not send the chat response"
        );
        Ok(())
    }

    pub async fn apply(&self, message: &ChatMessage, action: &Action, reason: &str) -> Result<()> {
        let mut query = vec![
            ("broadcaster_id", message.broadcaster_user_id.as_str()),
            ("moderator_id", self.user_id.as_str()),
        ];
        match action {
            Action::Delete | Action::Strike => {
                // Always supply the message ID: omission would clear the whole channel.
                ensure!(!message.message_id.is_empty(), "missing Twitch message ID");
                query.push(("message_id", &message.message_id));
                self.request(Method::DELETE, "/moderation/chat", &query, None)
                    .await?;
            }
            Action::Ban | Action::Timeout(_) => {
                self.request(
                    Method::POST,
                    "/moderation/bans",
                    &query,
                    Some(ban_body(&message.chatter_user_id, action, reason)?),
                )
                .await?;
            }
        }
        Ok(())
    }
}

pub(super) fn ban_body(user_id: &str, action: &Action, reason: &str) -> Result<Value> {
    let mut data =
        json!({"user_id": user_id, "reason": reason.chars().take(500).collect::<String>()});
    match action {
        Action::Ban => {}
        Action::Timeout(seconds) => {
            ensure!(
                (1..=1_209_600).contains(seconds),
                "invalid Twitch timeout duration"
            );
            data["duration"] = json!(seconds);
        }
        _ => anyhow::bail!("expected a ban or timeout"),
    }
    Ok(json!({"data": data}))
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    if !response.status().is_success() {
        // Response bodies can contain user input; never include them or credentials in logs.
        return Err(ApiError(response.status()).into());
    }
    Ok(response)
}
