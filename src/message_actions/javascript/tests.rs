use super::super::{tests::context, *};

use std::time::Duration;
use twilight_model::id::Id;

async fn process(codes: &[&str]) -> MessageActionReport<MessageActionOutcome> {
    let mut input = context(&(1..=codes.len() as i32).collect::<Vec<_>>());
    for (action, code) in input.actions.iter_mut().zip(codes) {
        action.code = Some((*code).into());
    }
    process_message(input, client()).await
}

fn client() -> typesafe::Client {
    // Code mode must not make a Jev request.
    typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1/v1")
        .max_retries(0)
        .build()
        .unwrap()
}

#[tokio::test]
async fn javascript_returns_all_four_outcomes_in_action_order() {
    let report = process(&[
        r#"function rule(messages) { return "BAN"; }"#,
        r#"(messages) => "KICK";"#,
        r#"messages => { return "STRIKE"; }"#,
        "messages => null",
    ])
    .await;
    assert_eq!((report.succeeded(), report.failed()), (4, 0));
    let outcomes = report
        .results
        .into_iter()
        .map(|result| (result.action_id, result.result.unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [
            (1, MessageActionOutcome::Ban("question-1".into())),
            (2, MessageActionOutcome::Kick("question-2".into())),
            (3, MessageActionOutcome::Strike("question-3".into())),
            (4, MessageActionOutcome::Ignore),
        ]
    );
}

#[tokio::test]
async fn javascript_receives_history_then_current_message_with_images_and_exact_ids() {
    let mut input = context(&[1]);
    input.history[0].id = 9_007_199_254_740_993;
    input.message.id = Id::new(9_007_199_254_740_995);
    input.images[0].attachment_id = 9_007_199_254_740_997;
    input.message.content = "\"); throw new Error('message is data'); //\n\u{2028}".into();
    let mut failed_image = input.images[0].clone();
    failed_image.description = None;
    failed_image.description_error = Some("unavailable".into());
    input.images.push(failed_image);
    input.actions[0].code = Some(
        r#"
        function (messages) {
            const previous = messages[0];
            const current = messages.at(-1);
            if (messages.length !== 2 ||
                previous.id !== "9007199254740993" ||
                current.id !== "9007199254740995" ||
                current.guild_id !== "100" || current.channel_id !== "200" ||
                previous.author_id !== "42" || current.author_id !== "42" ||
                previous.content !== "previous message" ||
                !current.content.includes("throw new Error") ||
                previous.timestamp !== 0 || current.timestamp !== 1767225600000 ||
                current.edited_timestamp !== null ||
                previous.images[0].description !== "A photo." ||
                current.images[0].attachment_id !== "9007199254740997" ||
                current.images[0].description !== "Incoming image text." ||
                current.images[1].description !== null ||
                current.images[1].description_error !== "unavailable") {
                throw new Error("wrong messages");
            }
            return "STRIKE";
        }
    "#
        .into(),
    );
    let report = process_message(input, client()).await;
    assert_eq!((report.succeeded(), report.failed()), (1, 0));
    assert_eq!(
        *report.results[0].result.as_ref().unwrap(),
        MessageActionOutcome::Strike("question-1".into())
    );
}

#[tokio::test]
async fn invalid_scripts_and_results_fail_only_their_own_action() {
    let report = process(&[
        "function (",
        "42",
        "() => { throw new Error('broken rule'); }",
        "() => undefined",
        "() => 'ban'",
        "() => true",
        "() => ({ action: 'BAN' })",
        "async () => 'BAN'",
        "() => null",
    ])
    .await;
    assert_eq!((report.succeeded(), report.failed()), (1, 8));
    for result in &report.results[..8] {
        assert!(matches!(result.result, Err(ActionError::Handler(_))));
    }
    assert!(
        report.results[2]
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("broken rule")
    );
    assert_eq!(
        *report.results[8].result.as_ref().unwrap(),
        MessageActionOutcome::Ignore
    );
}

#[tokio::test]
async fn scripts_have_separate_state_and_no_host_access() {
    let code = r#"messages => {
        if ([typeof process, typeof require, typeof fetch, typeof Deno, typeof std, typeof os]
            .some(type => type !== "undefined")) throw new Error("host access");
        if (globalThis.alreadyRan || messages[0].content !== "previous message")
            throw new Error("shared state");
        globalThis.alreadyRan = true;
        messages[0].content = "changed";
        return null;
    }"#;
    let report = process(&[code, code, code]).await;
    assert_eq!((report.succeeded(), report.failed()), (3, 0));
}

#[tokio::test]
async fn looping_script_is_interrupted_without_blocking_tokio() {
    let processor = tokio::spawn(process(&[
        "() => { while (true) { try { while (true) {} } catch (_) {} } }",
        "() => 'KICK'",
    ]));
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!processor.is_finished());
    let report = tokio::time::timeout(Duration::from_secs(5), processor)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((report.succeeded(), report.failed()), (1, 1));
    assert!(
        report.results[0]
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("execution limit")
    );
    assert_eq!(
        *report.results[1].result.as_ref().unwrap(),
        MessageActionOutcome::Kick("question-2".into())
    );
}

#[tokio::test]
async fn excessive_allocations_and_recursion_are_bounded() {
    let report = process(&[
        "() => { new ArrayBuffer(128 * 1024 * 1024); return 'BAN'; }",
        "function recurse(messages) { return recurse(messages); }",
        "() => null",
    ])
    .await;
    assert_eq!((report.succeeded(), report.failed()), (1, 2));
    assert!(report.results[0].result.is_err());
    assert!(report.results[1].result.is_err());
    assert_eq!(
        *report.results[2].result.as_ref().unwrap(),
        MessageActionOutcome::Ignore
    );
}
#[tokio::test]
async fn validates_synchronous_function_shape_without_calling_body() {
    for code in [
        "function (messages) { throw new Error('not called'); }",
        "strikes => null",
    ] {
        super::validate(code.into()).await.unwrap();
    }
    for code in [
        "42",
        "function (",
        "async messages => null",
        "function* (messages) { yield null; }",
        "() => null",
        "x",
    ] {
        assert!(super::validate(code.into()).await.is_err(), "{code}");
    }
    assert!(
        super::validate("(() => { while (true) {} })()".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("execution limit")
    );
    assert!(super::validate(" ".repeat(65 * 1024)).await.is_err());
}
