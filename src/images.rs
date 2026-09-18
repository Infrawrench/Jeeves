//! Image descriptions shared by message storage and action processing.

use serde::{Deserialize, Serialize};

/// A completed image attempt. A failed attempt has a description_error instead
/// of a description; both are preserved alongside the message in PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageResult {
    pub attachment_id: i64,
    pub url: String,
    pub mime_type: String,
    pub description: Option<String>,
    pub description_error: Option<String>,
}
