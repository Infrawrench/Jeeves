//! Typed async client for the [TypeSafe System One API](https://docs.typesafe.ai/api).
//!
//! ```no_run
//! use jeeves::typesafe::{Client, Question};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::from_env()?;
//! let question = Question::noul("Does this message contain spam?");
//! let result = client.ask("Buy followers now!", &question).await?;
//! println!("Spam probability: {}", result.answer.noul.get());
//! # Ok(()) }
//! ```

mod batch;
mod client;
mod error;
mod types;

pub use batch::{AnswerKey, Batch, BatchResponse, Evaluation};
pub use client::{Client, ClientBuilder, DEFAULT_BASE_URL, DEFAULT_MODEL};
pub use error::{ApiError, Error, Result};
pub use types::{
    ChoiceAnswer, Description, Model, ModelsResponse, NoulAnswer, Probability, Question,
    ScoreAnswer, Usage,
};

#[cfg(test)]
mod tests;
