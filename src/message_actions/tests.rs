use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::{Barrier, Semaphore, mpsc};

use super::*;

pub(super) fn context(ids: &[i32]) -> MessageContext {
    MessageContext {
        message: serde_json::from_value(json!({
            "id": "300", "guild_id": "100", "channel_id": "200",
            "author": {"id": "42", "username": "test", "discriminator": "0"},
            "content": "incoming", "timestamp": "2026-01-01T00:00:00+00:00",
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0
        }))
        .unwrap(),
        images: vec![image(500, "Incoming image text.")],
        history: vec![StoredMessage {
            id: 299,
            guild_id: 100,
            channel_id: 200,
            author_id: 42,
            content: "previous message".into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            edited_timestamp: None,
            images: vec![image(499, "A photo.")],
        }],
        actions: ids
            .iter()
            .map(|id| MessageAction {
                id: *id,
                guild_id: 100,
                only_channels: None,
                role_id: None,
                question: format!("question-{id}"),
                code: Some("messages => null".into()),
            })
            .collect(),
    }
}

fn image(id: i64, description: &str) -> ImageResult {
    ImageResult {
        attachment_id: id,
        url: "https://cdn.discordapp.com/photo.png".into(),
        mime_type: "image/png".into(),
        description: Some(description.into()),
        description_error: None,
    }
}

#[tokio::test]
async fn actions_run_in_parallel_share_history_and_reconcile_in_input_order() -> Result<()> {
    let gates: Arc<HashMap<_, _>> = Arc::new([10, 20, 30].map(|id| (id, Semaphore::new(0))).into());
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
    let processor = tokio::spawn(process_message_with_handler(context(&[30, 10, 20]), {
        let gates = Arc::clone(&gates);
        move |context, action| {
            let gates = Arc::clone(&gates);
            let started_tx = started_tx.clone();
            let finished_tx = finished_tx.clone();
            async move {
                assert_eq!(context.message.content, "incoming");
                assert_eq!(context.history[0].content, "previous message");
                assert_eq!(
                    context.history[0].images[0].description.as_deref(),
                    Some("A photo.")
                );
                assert_eq!(
                    context.images[0].description.as_deref(),
                    Some("Incoming image text.")
                );
                assert_eq!(action.question, format!("question-{}", action.id));
                assert_eq!(action.code.as_deref(), Some("messages => null"));
                started_tx.send((action.id, context))?;
                gates[&action.id].acquire().await?.forget();
                finished_tx.send(action.id)?;
                Ok(action.question)
            }
        }
    }));

    // Every task must start before any can finish: a serial implementation stalls here.
    let mut started = Vec::new();
    for _ in 0..3 {
        started.push(
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await?
                .unwrap(),
        );
    }
    assert!(Arc::ptr_eq(&started[0].1, &started[1].1));
    assert!(Arc::ptr_eq(&started[0].1, &started[2].1));
    assert!(!processor.is_finished());
    for id in [20, 30] {
        gates[&id].add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), finished_rx.recv()).await?,
            Some(id)
        );
        assert!(!processor.is_finished());
    }
    gates[&10].add_permits(1);
    let report = tokio::time::timeout(Duration::from_secs(5), processor).await??;
    assert_eq!(report.message_id.get(), 300);
    assert_eq!((report.succeeded(), report.failed()), (3, 0));
    assert_eq!(
        report
            .results
            .iter()
            .map(|result| result.action_id)
            .collect::<Vec<_>>(),
        [30, 10, 20]
    );
    assert_eq!(
        report
            .results
            .into_iter()
            .map(|result| result.result.unwrap())
            .collect::<Vec<_>>(),
        ["question-30", "question-10", "question-20"]
    );
    Ok(())
}

#[tokio::test]
async fn failures_and_panics_keep_their_action_ids_and_do_not_discard_siblings() -> Result<()> {
    let barrier = Arc::new(Barrier::new(3));
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        process_message_with_handler(context(&[11, 22, 33]), move |_, action| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
                match action.id {
                    11 => anyhow::bail!("expected action error"),
                    22 => panic!("expected action panic"),
                    _ => {
                        tokio::task::yield_now().await;
                        Ok(123)
                    }
                }
            }
        }),
    )
    .await?;
    assert_eq!((report.succeeded(), report.failed()), (1, 2));
    assert_eq!(
        report
            .results
            .iter()
            .map(|result| result.action_id)
            .collect::<Vec<_>>(),
        [11, 22, 33]
    );
    assert!(
        matches!(&report.results[0].result, Err(ActionError::Handler(error)) if error.to_string() == "expected action error")
    );
    assert!(matches!(&report.results[1].result, Err(ActionError::Task(error)) if error.is_panic()));
    assert_eq!(*report.results[2].result.as_ref().unwrap(), 123);
    Ok(())
}

#[tokio::test]
async fn panics_while_creating_the_handler_future_are_also_captured() {
    let report = process_message_with_handler(context(&[1, 2]), |_, action| {
        assert_ne!(action.id, 1, "expected synchronous handler panic");
        async { Ok(()) }
    })
    .await;
    assert_eq!((report.succeeded(), report.failed()), (1, 1));
    assert!(matches!(&report.results[0].result, Err(ActionError::Task(error)) if error.is_panic()));
    assert!(report.results[1].result.is_ok());
}

#[tokio::test]
async fn default_processor_reports_all_actions_and_handles_empty_lists() {
    let jev = typesafe::Client::new("test-key").unwrap();
    for ids in [vec![], vec![7, 3]] {
        let report = process_message(context(&ids), jev.clone()).await;
        assert_eq!(report.succeeded(), ids.len());
        assert_eq!(report.failed(), 0);
        assert!(
            report
                .results
                .iter()
                .all(|result| { matches!(result.result, Ok(MessageActionOutcome::Ignore)) })
        );
        assert_eq!(
            report
                .results
                .iter()
                .map(|result| result.action_id)
                .collect::<Vec<_>>(),
            ids
        );
    }
}

#[tokio::test]
async fn rules_without_code_evaluate_context_and_return_moderation_outcomes() -> Result<()> {
    for (probabilities, expected) in [
        (
            [1.0, 0.0, 0.0, 0.0],
            MessageActionOutcome::Ban("question-7".into()),
        ),
        (
            [0.0, 1.0, 0.0, 0.0],
            MessageActionOutcome::Kick("question-7".into()),
        ),
        (
            [0.0, 0.0, 1.0, 0.0],
            MessageActionOutcome::Strike("question-7".into()),
        ),
        ([0.0, 0.0, 0.0, 1.0], MessageActionOutcome::Ignore),
        // The weighted average is 1.35, but ban has the highest probability.
        (
            [0.45, 0.1, 0.1, 0.35],
            MessageActionOutcome::Ban("question-7".into()),
        ),
        ([0.5, 0.0, 0.0, 0.5], MessageActionOutcome::Ignore),
        (
            [0.0, 0.5, 0.5, 0.0],
            MessageActionOutcome::Strike("question-7".into()),
        ),
    ] {
        let score: f64 = probabilities
            .iter()
            .enumerate()
            .map(|(level, probability)| level as f64 * probability)
            .sum();
        let (jev, request) = jev_response(
            200,
            json!({
                "model": "jev-test", "usage": {},
                "answers": {"answer": {
                    "type": "score", "score": score, "confidence": 0.9,
                    "legend": {"0": "ban", "1": "kick", "2": "strike", "3": "no action"},
                    "probabilities": {
                        "0": probabilities[0], "1": probabilities[1],
                        "2": probabilities[2], "3": probabilities[3],
                    },
                }},
            }),
        )?;
        let mut context = context(&[7, 3]);
        context.actions[0].code = None;
        let mut failed_image = image(501, "");
        failed_image.description = None;
        failed_image.description_error = Some("image unavailable".into());
        context.images.push(failed_image);
        let report = process_message(context, jev).await;
        assert_eq!((report.succeeded(), report.failed()), (2, 0));
        let mut results = report.results.into_iter();
        let first = results.next().unwrap();
        assert_eq!(first.action_id, 7);
        assert_eq!(first.result.unwrap(), expected);
        let second = results.next().unwrap();
        assert_eq!(second.action_id, 3);
        assert_eq!(second.result.unwrap(), MessageActionOutcome::Ignore);

        let request = request.join().unwrap();
        let question = &request["questions"]["answer"];
        assert_eq!(question["type"], "score");
        assert_eq!(
            question["criteria"],
            json!(["ban", "kick", "strike", "no action"])
        );
        let instructions = question["instructions"].as_str().unwrap();
        assert!(instructions.contains("ONLY current_message and its author"));
        assert!(instructions.contains("never act solely because an older message matched"));
        assert!(instructions.contains("ban means ban, kick means kick, strike means strike"));
        assert!(instructions.contains("Default to strike ONLY if the rule specifies no outcome"));
        assert!(instructions.ends_with("Rule: question-7"));
        let state = &request["state"];
        assert_eq!(state["guild_id"], "100");
        assert_eq!(state["channel_id"], "200");
        assert_eq!(state["current_message"]["id"], "300");
        assert_eq!(state["current_message"]["author_id"], "42");
        assert_eq!(state["current_message"]["content"], "incoming");
        assert_eq!(
            state["current_message"]["images"][0]["description"],
            "Incoming image text."
        );
        assert_eq!(
            state["current_message"]["images"][1]["description_error"],
            "image unavailable"
        );
        assert_eq!(state["history"][0]["id"], "299");
        assert_eq!(state["history"][0]["content"], "previous message");
        assert_eq!(state["history"][0]["images"][0]["description"], "A photo.");
    }
    Ok(())
}

#[tokio::test]
async fn jev_failure_is_reported_without_discarding_other_action_results() -> Result<()> {
    let (jev, request) = jev_response(503, json!({"error": "unavailable"}))?;
    let mut context = context(&[7, 3]);
    context.actions[0].code = None;
    let report = process_message(context, jev).await;
    assert_eq!((report.succeeded(), report.failed()), (1, 1));
    assert_eq!(report.results[0].action_id, 7);
    assert!(matches!(
        &report.results[0].result,
        Err(ActionError::Handler(error))
            if matches!(error.downcast_ref::<typesafe::Error>(), Some(typesafe::Error::Api(_)))
    ));
    assert_eq!(report.results[1].action_id, 3);
    assert_eq!(
        *report.results[1].result.as_ref().unwrap(),
        MessageActionOutcome::Ignore
    );
    request.join().unwrap();
    Ok(())
}

pub(crate) fn role_score_response(strike: bool, level: usize) -> Value {
    let criteria = action_criteria(strike, Some(9007199254740993));
    let legend: serde_json::Map<_, _> = criteria
        .iter()
        .enumerate()
        .map(|(index, name)| (index.to_string(), json!(name)))
        .collect();
    let probabilities: serde_json::Map<_, _> = criteria
        .iter()
        .enumerate()
        .map(|(index, _)| (index.to_string(), json!(usize::from(index == level))))
        .collect();
    json!({"model": "jev-test", "usage": {}, "answers": {"answer": {
        "type": "score", "score": level, "confidence": 1,
        "legend": legend, "probabilities": probabilities,
    }}})
}

#[tokio::test]
async fn message_role_outcomes_use_the_configured_role_in_both_modes() -> Result<()> {
    for (level, code, expected) in [
        (
            3,
            "'GIVE_ROLE'",
            MessageActionOutcome::GiveRole {
                role_id: 9007199254740993,
                reason: "question-7".into(),
            },
        ),
        (
            4,
            "'REVOKE_ROLE'",
            MessageActionOutcome::RevokeRole {
                role_id: 9007199254740993,
                reason: "question-7".into(),
            },
        ),
        (5, "null", MessageActionOutcome::Ignore),
    ] {
        let (jev, request) = jev_response(200, role_score_response(false, level))?;
        let mut input = context(&[7]);
        input.actions[0].code = None;
        input.actions[0].role_id = Some(9007199254740993);
        let report = process_message(input, jev).await;
        assert_eq!(*report.results[0].result.as_ref().unwrap(), expected);
        let request = request.join().unwrap();
        assert_eq!(
            request["questions"]["answer"]["criteria"],
            json!([
                "ban",
                "kick",
                "strike",
                "give role",
                "revoke role",
                "no action"
            ])
        );

        let mut input = context(&[7]);
        input.actions[0].code = Some(format!("messages => {code}"));
        input.actions[0].role_id = Some(9007199254740993);
        let jev = typesafe::Client::builder("test-key")
            .base_url("http://127.0.0.1:1/v1")
            .build()?;
        let report = process_message(input, jev).await;
        assert_eq!(*report.results[0].result.as_ref().unwrap(), expected);
    }
    Ok(())
}

#[test]
fn jev_history_fits_exactly_and_reserves_each_question_and_json_overhead() -> Result<()> {
    let input = context(&[]);
    let mut context = ActionContext {
        message: input.message,
        images: input.images,
        history: input.history,
    };
    context.message.content = "Current: 🦀\n\"quoted\" \\".into();
    context.images[0].description = Some("Image: 日本語\n\"quoted\"".into());
    let mut older = self::context(&[]).history.pop().unwrap();
    older.id = 298;
    older.images[0].description_error = Some("Unavailable: \"画像\"\n".into());
    context.history.insert(0, older);
    let rule = "Strike for \"spam\"\n日本語";
    let question = moderation_question(rule, None)?;
    let small = jev_state(&context, &question)?;
    assert_eq!(small["history"].as_array().unwrap().len(), 2);
    let remaining = JEV_CONTEXT_BUDGET
        - JEV_CONTEXT_RESERVE
        - question.json_size()?
        - serde_json::to_vec(&small)?.len();
    context.history[1].content.push_str(&"a".repeat(remaining));

    let exact = jev_state(&context, &question)?;
    assert_eq!(exact["history"][0]["id"], "299");
    assert_eq!(exact["history"][1]["id"], "298");
    assert_eq!(
        serde_json::to_vec(&exact)?.len() + question.json_size()? + JEV_CONTEXT_RESERVE,
        JEV_CONTEXT_BUDGET
    );
    assert_eq!(exact["current_message"]["content"], context.message.content);
    assert_eq!(exact["current_message"]["images"], json!(context.images));

    // The same message history fits differently for a longer rule.
    let longer_question = moderation_question(&format!("{rule}!"), None)?;
    assert_eq!(
        jev_state(&context, &longer_question)?["history"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    context.history[1].content.push('a');
    let over = jev_state(&context, &question)?;
    assert_eq!(over["history"].as_array().unwrap().len(), 1);
    assert_eq!(over["history"][0]["id"], "299");
    assert_eq!(over["history"][0]["content"], context.history[1].content);
    Ok(())
}

#[test]
fn jev_preserves_oversized_current_message_and_images_without_history() -> Result<()> {
    let input = context(&[]);
    let mut context = ActionContext {
        message: input.message,
        images: input.images,
        history: input.history,
    };
    context.message.content = "Current 🦀\n".repeat(4_000);
    context.images[0].description = Some("Image text\n".repeat(4_000));
    let question = moderation_question("Strike for spam", None)?;
    let state = jev_state(&context, &question)?;
    assert_eq!(state["current_message"]["id"], "300");
    assert_eq!(state["current_message"]["content"], context.message.content);
    assert_eq!(state["current_message"]["images"], json!(context.images));
    assert_eq!(state["history"], json!([]));
    Ok(())
}

#[tokio::test]
async fn jev_sends_recent_history_until_full_while_code_receives_all_messages() -> Result<()> {
    let (jev, request) = jev_response(
        200,
        json!({
            "model": "jev-test", "usage": {},
            "answers": {"answer": {
                "type": "score", "score": 3.0, "confidence": 1.0,
                "legend": {"0": "ban", "1": "kick", "2": "strike", "3": "no action"},
                "probabilities": {"0": 0.0, "1": 0.0, "2": 0.0, "3": 1.0},
            }},
        }),
    )?;
    let mut input = context(&[7, 3]);
    input.actions[0].code = None;
    input.actions[1].code = Some(
        "messages => {
            if (messages.length !== 11 || messages[0].id !== '290' ||
                messages[9].id !== '299' || messages[10].id !== '300') {
                throw new Error('Full chronological history is required');
            }
            return null;
        }"
        .into(),
    );
    input.history = (290..300)
        .map(|id| {
            let mut previous = context(&[]).history.pop().unwrap();
            previous.id = id;
            previous.content = match id {
                297 => "too large".repeat(4_000),
                298 | 299 => "🦀\n\"text\"\\".repeat(500),
                _ => "small older message".into(),
            };
            previous
        })
        .collect();
    let report = process_message(input, jev).await;
    assert_eq!((report.succeeded(), report.failed()), (2, 0));
    let request = request.join().unwrap();
    let state = &request["state"];
    assert_eq!(state["current_message"]["content"], "incoming");
    assert_eq!(
        state["current_message"]["images"][0]["description"],
        "Incoming image text."
    );
    let history = state["history"].as_array().unwrap();
    // Stop at the first non-fitting message; don't skip it to include older ones.
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["id"], "299");
    assert_eq!(history[1]["id"], "298");
    assert!(
        serde_json::to_vec(state)?.len()
            + serde_json::to_vec(&request["questions"]["answer"])?.len()
            + JEV_CONTEXT_RESERVE
            <= JEV_CONTEXT_BUDGET
    );
    Ok(())
}

pub(crate) fn jev_response(
    status: u16,
    body: Value,
) -> Result<(typesafe::Client, thread::JoinHandle<Value>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let client = typesafe::Client::builder("test-key")
        .base_url(format!("http://{}/v1", listener.local_addr()?))
        .timeout(Duration::from_secs(5))
        .max_retries(0)
        .build()?;
    let task = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "missing Jev request");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let end = loop {
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/systemone "));
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .parse()
            .unwrap();
        while bytes.len() < end + length {
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let request = serde_json::from_slice(&bytes[end..end + length]).unwrap();
        let body = body.to_string();
        write!(stream,
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ).unwrap();
        request
    });
    Ok((client, task))
}

#[tokio::test]
async fn canceling_the_parent_cancels_its_action_tasks() -> Result<()> {
    struct OnDrop(i32, mpsc::UnboundedSender<i32>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.1.send(self.0);
        }
    }

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();
    let processor = tokio::spawn(process_message_with_handler(
        context(&[1, 2, 3]),
        move |_, action| {
            let started_tx = started_tx.clone();
            let dropped_tx = dropped_tx.clone();
            async move {
                let _guard = OnDrop(action.id, dropped_tx);
                started_tx.send(action.id)?;
                std::future::pending::<Result<()>>().await
            }
        },
    ));
    for _ in 0..3 {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await?
                .is_some()
        );
    }
    processor.abort();
    assert!(processor.await.unwrap_err().is_cancelled());
    let mut dropped = Vec::new();
    for _ in 0..3 {
        dropped.push(
            tokio::time::timeout(Duration::from_secs(5), dropped_rx.recv())
                .await?
                .unwrap(),
        );
    }
    dropped.sort_unstable();
    assert_eq!(dropped, [1, 2, 3]);
    Ok(())
}
