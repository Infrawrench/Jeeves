use std::{fmt, time::Duration};

pub type Result<T> = std::result::Result<T, Error>;

/// Errors can be matched without inspecting their human-readable messages.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid TypeSafe configuration: {0}")]
    Configuration(&'static str),
    #[error("invalid TypeSafe request: {0}")]
    InvalidRequest(&'static str),
    #[error("invalid TypeSafe response: {0}")]
    InvalidResponse(&'static str),
    #[error("failed to encode TypeSafe request: {0}")]
    Encode(serde_json::Error),
    #[error("failed to decode TypeSafe response: {0}")]
    Decode(serde_json::Error),
    #[error("TypeSafe request timed out")]
    Timeout,
    #[error("TypeSafe HTTP request failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("TypeSafe response exceeds the configured size limit")]
    ResponseTooLarge,
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Transport(error.without_url())
        }
    }
}

/// HTTP failure details. Response bodies are available explicitly, but omitted
/// from Display/Debug because validation errors may echo submitted content.
#[derive(thiserror::Error)]
#[error("TypeSafe returned HTTP {status}")]
pub struct ApiError {
    pub status: reqwest::StatusCode,
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
    pub(super) body: String,
}

impl ApiError {
    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiError")
            .field("status", &self.status)
            .field("request_id", &self.request_id)
            .field("retry_after", &self.retry_after)
            .finish_non_exhaustive()
    }
}
