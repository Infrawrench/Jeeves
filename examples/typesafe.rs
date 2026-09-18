//! Run explicitly with `cargo run --example typesafe` to evaluate a sample message.
use anyhow::Result;
use jeeves::typesafe::{Batch, Client, Question};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Allow,
    Review,
}

#[tokio::main]
async fn main() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(error) if error.not_found() => {}
        Err(error) => return Err(error.into()),
    }
    let client = Client::from_env()?;
    let mut questions = Batch::new();
    let action = questions.add(
        "action",
        Question::choice(
            "Should a moderator review this message?",
            [
                (Action::Allow, "Ordinary conversation"),
                (Action::Review, "Potential spam or harassment"),
            ],
        )?,
    )?;
    let spam = questions.add(
        "spam",
        Question::noul("Does this message contain unsolicited advertising?"),
    )?;
    let severity = questions.add(
        "severity",
        Question::score(
            "Rate the severity of the rule violation",
            ["No violation", "Minor violation", "Serious violation"],
        )?,
    )?;
    let response = client
        .evaluate(
            &json!({"message": "Buy followers now! DM me for prices."}),
            &questions,
        )
        .await?;
    // The compiler knows this is an Action enum, not a String or a score.
    match response.get(&action)?.choice {
        Action::Allow => println!("Suggested action: allow"),
        Action::Review => println!("Suggested action: moderator review"),
    }
    println!("Spam probability: {}", response.get(&spam)?.noul.get());
    println!("Severity: {}", response.get(&severity)?.score);
    println!("Model: {}; usage: {:?}", response.model, response.usage);
    Ok(())
}
