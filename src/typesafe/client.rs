use std::{
    fmt,
    time::{Duration, SystemTime},
};

use reqwest::{
    Method,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use url::{Host, Url};

use super::{ApiError, Batch, BatchResponse, Error, Evaluation, ModelsResponse, Question, Result};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1/";
pub const DEFAULT_MODEL: &str = "jev-latest";

/// Cheaply cloneable async client. Reuses pooled connections and verified TLS.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    authorization: HeaderValue,
    base_url: Url,
    model: String,
    timeout: Duration,
    max_retries: u32,
    max_response_bytes: usize,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypeSafeClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::builder(api_key).build()
    }

    /// Reads TYPESAFE_API_KEY and optional TYPESAFE_MODEL. Does not load .env itself.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("TYPESAFE_API_KEY").map_err(|_| {
            Error::Configuration("TYPESAFE_API_KEY is required and must be valid Unicode")
        })?;
        let builder = Self::builder(key);
        match std::env::var("TYPESAFE_MODEL") {
            Ok(model) => builder.model(model).build(),
            Err(std::env::VarError::NotPresent) => builder.build(),
            Err(_) => Err(Error::Configuration("TYPESAFE_MODEL must be valid Unicode")),
        }
    }

    pub fn builder(api_key: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            timeout: Duration::from_secs(30),
            max_retries: 2,
            max_response_bytes: 4 * 1024 * 1024,
        }
    }

    pub async fn ask<S: Serialize + ?Sized, A: DeserializeOwned>(
        &self,
        state: &S,
        question: &Question<A>,
    ) -> Result<Evaluation<A>> {
        let mut batch = Batch::new();
        let key = batch.add("answer", question.clone())?;
        let response = self.evaluate(state, &batch).await?;
        Ok(Evaluation {
            answer: response.get(&key)?,
            model: response.model,
            usage: response.usage,
            request_id: response.request_id,
        })
    }

    /// Evaluates mixed question types in one request. State can be any serializable
    /// string, object, or array, including a domain struct or a list of messages.
    pub async fn evaluate<S: Serialize + ?Sized>(
        &self,
        state: &S,
        batch: &Batch,
    ) -> Result<BatchResponse> {
        if batch.questions.is_empty() {
            return Err(Error::InvalidRequest(
                "a batch must contain at least one question",
            ));
        }
        let state = serde_json::to_value(state).map_err(Error::Encode)?;
        if !matches!(state, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            return Err(Error::InvalidRequest(
                "state must serialize as a string, object, or array",
            ));
        }
        let body = serde_json::to_vec(&json!({
            "model": self.model, "state": state, "questions": batch.questions,
        }))
        .map_err(Error::Encode)?;
        let (wire, request_id) = self.request(Method::POST, "systemone", Some(body)).await?;
        BatchResponse::from_wire(wire, batch, request_id)
    }

    pub async fn models(&self) -> Result<ModelsResponse> {
        let (mut response, request_id): (ModelsResponse, _) =
            self.request(Method::GET, "models", None).await?;
        response.request_id = request_id;
        Ok(response)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(T, Option<String>)> {
        // The budget includes all attempts, response reads, and retry delays.
        tokio::time::timeout(self.timeout, async {
            let url = self
                .base_url
                .join(path)
                .map_err(|_| Error::Configuration("invalid endpoint URL"))?;
            for attempt in 0..=self.max_retries {
                let mut request = self
                    .http
                    .request(method.clone(), url.clone())
                    .header(AUTHORIZATION, self.authorization.clone());
                if let Some(body) = &body {
                    request = request
                        .header(CONTENT_TYPE, "application/json")
                        .body(body.clone());
                }
                // Connection errors and timeouts are returned directly: a POST may already
                // have been evaluated, and retrying could incur duplicate usage.
                let mut response = request.send().await?;
                let status = response.status();
                let request_id = response
                    .headers()
                    .get("x-typesafe-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let retry_after = retry_after(response.headers());
                if response
                    .content_length()
                    .is_some_and(|size| size > self.max_response_bytes as u64)
                {
                    return Err(Error::ResponseTooLarge);
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    if chunk.len() > self.max_response_bytes - bytes.len() {
                        return Err(Error::ResponseTooLarge);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if status.is_success() {
                    return Ok((
                        serde_json::from_slice(&bytes).map_err(Error::Decode)?,
                        request_id,
                    ));
                }
                if attempt < self.max_retries
                    && matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529)
                {
                    let backoff = Duration::from_secs_f64(
                        (0.5 * 2_f64.powi(attempt.min(4) as i32)).min(8.0)
                            * (0.5 + fastrand::f64() * 0.5),
                    );
                    tokio::time::sleep(retry_after.unwrap_or(backoff)).await;
                    continue;
                }
                return Err(ApiError {
                    status,
                    request_id,
                    retry_after,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                }
                .into());
            }
            unreachable!("the final attempt always returns")
        })
        .await
        .map_err(|_| Error::Timeout)?
    }
}

/// Client configuration. Credentials are intentionally excluded from Debug.
pub struct ClientBuilder {
    api_key: String,
    base_url: String,
    model: String,
    timeout: Duration,
    max_retries: u32,
    max_response_bytes: usize,
}

impl ClientBuilder {
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// API root including /v1. HTTPS is required except for loopback test servers.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Total time budget for an API call, including retries. Defaults to 30 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Retries after the initial attempt; defaults to two. Zero disables retries.
    pub fn max_retries(mut self, count: u32) -> Self {
        self.max_retries = count;
        self
    }

    pub fn max_response_bytes(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit;
        self
    }

    pub fn build(self) -> Result<Client> {
        if self.api_key.trim().is_empty() {
            return Err(Error::Configuration("API key must not be empty"));
        }
        if self.model.trim().is_empty() {
            return Err(Error::Configuration("model must not be empty"));
        }
        if self.timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.timeout)
                .is_none()
            || self.max_response_bytes == 0
        {
            return Err(Error::Configuration(
                "timeout and response size limit must be positive and finite",
            ));
        }
        let mut base_url =
            Url::parse(&self.base_url).map_err(|_| Error::Configuration("invalid base URL"))?;
        let loopback = match base_url.host() {
            Some(Host::Domain("localhost")) => true,
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(base_url.scheme() == "https" || base_url.scheme() == "http" && loopback)
            || base_url.host().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(Error::Configuration(
                "base URL must use HTTPS (or loopback HTTP), without credentials, query, or fragment",
            ));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.trim()))
                .map_err(|_| Error::Configuration("API key contains invalid header characters"))?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.timeout.min(Duration::from_secs(10)))
            .timeout(self.timeout)
            .user_agent(concat!("jeeves-typesafe/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Client {
            http,
            authorization,
            base_url,
            model: self.model,
            timeout: self.timeout,
            max_retries: self.max_retries,
            max_response_bytes: self.max_response_bytes,
        })
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let millis = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(|value| Duration::try_from_secs_f64(value / 1000.0).ok());
    millis.or_else(|| {
        let value = headers.get("retry-after")?.to_str().ok()?;
        if let Ok(seconds) = value.parse::<f64>() {
            Duration::try_from_secs_f64(seconds).ok()
        } else {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_headers_accept_seconds_milliseconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("2"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert("retry-after-ms", HeaderValue::from_static("250"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(250)));
        headers.remove("retry-after-ms");
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        for value in ["-1", "NaN", "inf", "not-a-date"] {
            headers.insert("retry-after", HeaderValue::from_str(value).unwrap());
            assert_eq!(retry_after(&headers), None);
        }
    }
}
