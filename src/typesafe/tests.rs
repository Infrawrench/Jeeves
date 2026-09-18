use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Allow,
    Review,
}

fn choice() -> Question<ChoiceAnswer<Decision>> {
    Question::choice(
        "Choose an action",
        [
            (Decision::Allow, "Fine"),
            (Decision::Review, "Needs review"),
        ],
    )
    .unwrap()
}

fn answers() -> Value {
    json!({
        "model": "jev-test", "usage": {"input_tokens": 42, "output_tokens": 7},
        "answers": {
            "spam": {"type": "noul", "noul": 0.8},
            "action": {"type": "choice", "choice": "review", "probabilities": {"allow": 0.2, "review": 0.8}, "confidence": 0.6},
            "severity": {"type": "score", "score": 1.5, "legend": {"0": "Low", "1": "Medium", "2": "High"}, "probabilities": {"0": 0.1, "1": 0.3, "2": 0.6}, "confidence": 0.5}
        }
    })
}

fn mixed_batch() -> (
    Batch,
    AnswerKey<NoulAnswer>,
    AnswerKey<ChoiceAnswer<Decision>>,
    AnswerKey<ScoreAnswer>,
) {
    let mut batch = Batch::new();
    let spam = batch
        .add(
            "spam",
            Question::noul("Is this spam?")
                .with_criteria("Unsolicited advertising", "Normal conversation"),
        )
        .unwrap();
    let action = batch.add("action", choice()).unwrap();
    let severity = batch
        .add(
            "severity",
            Question::score("Severity?", ["Low", "Medium", "High"]).unwrap(),
        )
        .unwrap();
    (batch, spam, action, severity)
}

#[test]
fn questions_serialize_to_the_documented_shapes_and_validate_input() {
    let (mut batch, _, _, _) = mixed_batch();
    let encoded = serde_json::to_value(&batch.questions).unwrap();
    assert_eq!(
        encoded["action"],
        json!({"type":"choice", "instructions":"Choose an action", "criteria":{"allow":"Fine", "review":"Needs review"}})
    );
    assert_eq!(
        encoded["spam"]["criteria"],
        json!({"true":"Unsolicited advertising", "false":"Normal conversation"})
    );
    assert_eq!(
        encoded["severity"]["criteria"],
        json!(["Low", "Medium", "High"])
    );
    assert!(batch.add("spam", choice()).is_err());
    assert_eq!(batch.questions.len(), 3);
    assert!(batch.add(" ", choice()).is_err());
    assert!(Question::score("rate", ["Only one level"]).is_err());
    assert!(Question::choice("choose", [(Decision::Allow, "a"), (Decision::Allow, "b")]).is_err());
    assert!(Question::choice("choose", [(123_u32, "not a string")]).is_err());
    assert!(Question::choice("choose", Vec::<(String, String)>::new()).is_err());

    let description =
        Description::try_from(json!({"rule": "No unsolicited ads", "examples": ["Buy now"]}))
            .unwrap();
    let question = Question::score(
        description.clone(),
        [Description::Null, description.clone()],
    )
    .unwrap();
    let encoded = serde_json::to_value(question.wire).unwrap();
    assert_eq!(
        encoded["instructions"],
        serde_json::to_value(description).unwrap()
    );
    assert_eq!(encoded["criteria"][0], Value::Null);
    assert!(Description::try_from(json!(true)).is_err());
    for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
        assert!(Probability::try_from(value).is_err());
    }
}

#[test]
fn mixed_answers_are_typed_and_keys_are_scoped_to_their_batch() {
    let (batch, spam, action, severity) = mixed_batch();
    let response = BatchResponse::from_wire(
        serde_json::from_value(answers()).unwrap(),
        &batch,
        Some("req-test".into()),
    )
    .unwrap();
    assert_eq!(response.get(&spam).unwrap().noul.get(), 0.8);
    let answer: ChoiceAnswer<Decision> = response.get(&action).unwrap();
    assert_eq!(answer.choice, Decision::Review);
    assert_eq!(answer.probabilities[&Decision::Allow].get(), 0.2);
    assert_eq!(response.get(&severity).unwrap().score, 1.5);
    let mut other = Batch::new();
    let other_key = other
        .add("spam", Question::noul("Unrelated question"))
        .unwrap();
    assert!(matches!(
        response.get(&other_key),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn malformed_or_mismatched_answers_are_rejected() {
    let (batch, _, _, _) = mixed_batch();
    let mutations = [
        ("/answers/spam/noul", json!(1.2)),
        (
            "/answers/spam",
            json!({"type":"choice", "choice":"x", "probabilities":{"x":1}, "confidence":1}),
        ),
        ("/answers/action/choice", json!("ban")),
        (
            "/answers/action/probabilities",
            json!({"allow":0.2, "ban":0.8}),
        ),
        ("/answers/action/probabilities/allow", json!(0.8)),
        ("/answers/action/confidence", json!(-1)),
        ("/answers/severity/score", json!(3)),
        ("/answers/severity/legend/0", json!("Wrong rubric")),
        ("/answers/severity/probabilities", json!({"0":0.1, "1":0.9})),
        ("/model", json!("")),
    ];
    for (pointer, value) in mutations {
        let mut wire = answers();
        *wire.pointer_mut(pointer).unwrap() = value;
        let response = serde_json::from_value(wire)
            .map_err(Error::Decode)
            .and_then(|wire| BatchResponse::from_wire(wire, &batch, None));
        assert!(response.is_err(), "{pointer}");
    }
    for remove in [true, false] {
        let mut wire = answers();
        if remove {
            wire["answers"].as_object_mut().unwrap().remove("spam");
        } else {
            wire["answers"]["extra"] = json!({"type":"noul", "noul":0.5});
        }
        assert!(
            BatchResponse::from_wire(serde_json::from_value(wire).unwrap(), &batch, None).is_err()
        );
    }
}

#[test]
fn client_configuration_is_validated_and_credentials_are_redacted() {
    assert!(Client::new("").is_err());
    assert!(Client::new("bad\r\nkey").is_err());
    assert!(Client::builder("key").model(" ").build().is_err());
    assert!(
        Client::builder("key")
            .timeout(Duration::ZERO)
            .build()
            .is_err()
    );
    assert!(
        Client::builder("key")
            .max_response_bytes(0)
            .build()
            .is_err()
    );
    for url in [
        "http://api.typesafe.ai/v1",
        "https://user:pass@example.com",
        "https://example.com/?key=secret",
        "https://example.com/#secret",
        "file:///tmp/key",
    ] {
        assert!(
            Client::builder("key").base_url(url).build().is_err(),
            "{url}"
        );
    }
    let client = Client::new("secret-test-key").unwrap();
    assert!(!format!("{client:?}").contains("secret-test-key"));
}

struct Stub {
    status: u16,
    body: String,
    headers: String,
    delay: Duration,
}

impl Stub {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body: body.to_string(),
            headers: "x-typesafe-request-id: req-test\r\n".into(),
            delay: Duration::ZERO,
        }
    }
}

struct Captured {
    headers: String,
    body: Value,
}

fn server(stubs: Vec<Stub>) -> (String, thread::JoinHandle<Vec<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let mut captured = Vec::new();
        for stub in stubs {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing mock request");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
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
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap_or("0")
                .parse()
                .unwrap();
            while bytes.len() < end + length {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[end..end + length]).unwrap()
            };
            captured.push(Captured { headers, body });
            thread::sleep(stub.delay);
            let (length_header, body) = if stub.headers.contains("Transfer-Encoding: chunked") {
                (
                    String::new(),
                    format!("{:x}\r\n{}\r\n0\r\n\r\n", stub.body.len(), stub.body),
                )
            } else {
                (
                    format!("Content-Length: {}\r\n", stub.body.len()),
                    stub.body,
                )
            };
            // A timeout or response-limit test can close the connection early.
            let _ = write!(
                stream,
                "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\n{}{}Connection: close\r\n\r\n{}",
                stub.status, length_header, stub.headers, body
            );
        }
        captured
    });
    (url, task)
}

#[tokio::test]
async fn http_batch_retries_rate_limits_and_preserves_typed_answers() -> Result<()> {
    let mut rate_limit = Stub::json(429, json!({"error":"rate limited"}));
    rate_limit.headers.push_str("retry-after-ms: 1\r\n");
    let mut overloaded = Stub::json(529, json!({"error":"overloaded"}));
    overloaded.headers.push_str("retry-after: 0\r\n");
    let (url, task) = server(vec![rate_limit, overloaded, Stub::json(200, answers())]);
    let client = Client::builder("test-key")
        .base_url(url)
        .model("pinned-model")
        .build()?;
    let (batch, spam, action, severity) = mixed_batch();
    let result = client
        .evaluate(&json!({"messages":[{"content":"Buy now"}]}), &batch)
        .await?;
    assert_eq!(result.get(&action)?.choice, Decision::Review);
    assert_eq!(result.get(&spam)?.noul.get(), 0.8);
    assert_eq!(result.get(&severity)?.score, 1.5);
    assert_eq!(result.request_id.as_deref(), Some("req-test"));
    assert_eq!(result.usage.input_tokens, Some(42));
    let requests = task.join().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert!(request.headers.starts_with("post /v1/systemone "));
        assert!(request.headers.contains("authorization: bearer test-key"));
        assert!(request.headers.contains("content-type: application/json"));
        assert_eq!(request.body["model"], "pinned-model");
        assert_eq!(
            request.body["questions"],
            serde_json::to_value(&batch.questions).unwrap()
        );
        assert_eq!(request.body["state"]["messages"][0]["content"], "Buy now");
        assert!(!request.body.to_string().contains("test-key"));
    }
    Ok(())
}

#[tokio::test]
async fn http_models_and_single_question_use_documented_endpoints() -> Result<()> {
    let (url, task) = server(vec![
        Stub::json(
            200,
            json!({"models":[{"name":"jev-latest", "description":"Stable", "release_date":"2026-09-01"}]}),
        ),
        Stub::json(
            200,
            json!({"model":"jev-test", "usage":{}, "answers":{"answer":{"type":"noul", "noul":0.9}}}),
        ),
    ]);
    let client = Client::builder("test-key").base_url(url).build()?;
    assert_eq!(client.models().await?.models[0].name, "jev-latest");
    let result = client
        .ask("Some text", &Question::noul("Is it text?"))
        .await?;
    let answer: NoulAnswer = result.answer;
    assert_eq!(answer.noul.get(), 0.9);
    assert!(result.usage.input_tokens.is_none());
    let requests = task.join().unwrap();
    assert!(requests[0].headers.starts_with("get /v1/models "));
    assert!(requests[0].body.is_null());
    assert_eq!(requests[1].body["model"], DEFAULT_MODEL);
    assert_eq!(requests[1].body["state"], "Some text");
    assert!(
        requests[1].body["questions"]["answer"]
            .get("criteria")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn http_errors_are_structured_and_auth_validation_and_redirects_are_not_retried() -> Result<()>
{
    let mut redirect = Stub::json(302, json!({"detail":"redirect"}));
    redirect
        .headers
        .push_str("Location: http://127.0.0.1:1/should-not-follow\r\n");
    let mut malformed = Stub::json(200, Value::Null);
    malformed.body = "not JSON".into();
    let (url, task) = server(vec![
        Stub::json(401, json!({"detail":"private request content"})),
        Stub::json(
            422,
            json!({"detail":[{"loc":["body","state"], "msg":"invalid"}]}),
        ),
        redirect,
        malformed,
    ]);
    let client = Client::builder("test-key").base_url(url).build()?;
    for expected in [401, 422, 302] {
        let error = client
            .ask("content", &Question::noul("question"))
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("private request content"));
        let Error::Api(error) = error else {
            panic!("expected API error")
        };
        assert_eq!(error.status.as_u16(), expected);
        assert_eq!(error.request_id.as_deref(), Some("req-test"));
        assert!(!error.body().is_empty());
    }
    assert!(matches!(
        client.ask("content", &Question::noul("question")).await,
        Err(Error::Decode(_))
    ));
    assert_eq!(task.join().unwrap().len(), 4);
    Ok(())
}

#[tokio::test]
async fn http_retries_stop_at_the_limit() -> Result<()> {
    let responses = (0..3)
        .map(|_| {
            let mut stub = Stub::json(429, json!({"error":"slow down"}));
            stub.headers.push_str("retry-after-ms: 1\r\n");
            stub
        })
        .collect();
    let (url, task) = server(responses);
    let client = Client::builder("test-key")
        .base_url(url)
        .max_retries(2)
        .build()?;
    let Error::Api(error) = client.models().await.unwrap_err() else {
        panic!("expected API error")
    };
    assert_eq!(error.status.as_u16(), 429);
    assert_eq!(error.retry_after, Some(Duration::from_millis(1)));
    assert_eq!(task.join().unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn http_timeouts_and_body_limits_are_enforced() -> Result<()> {
    let mut delayed = Stub::json(200, json!({"models":[]}));
    delayed.delay = Duration::from_millis(150);
    let (url, task) = server(vec![delayed]);
    let client = Client::builder("test-key")
        .base_url(url)
        .timeout(Duration::from_millis(75))
        .build()?;
    assert!(matches!(client.models().await, Err(Error::Timeout)));
    task.join().unwrap();
    for chunked in [false, true] {
        let mut oversized = Stub::json(200, json!({"models":[],"extra":"x".repeat(100)}));
        if chunked {
            oversized.headers.push_str("Transfer-Encoding: chunked\r\n");
        }
        let (url, task) = server(vec![oversized]);
        let client = Client::builder("test-key")
            .base_url(url)
            .max_response_bytes(32)
            .build()?;
        assert!(matches!(
            client.models().await,
            Err(Error::ResponseTooLarge)
        ));
        task.join().unwrap();
    }
    Ok(())
}

#[tokio::test]
async fn invalid_state_and_empty_batches_fail_before_http() -> Result<()> {
    let client = Client::builder("test-key")
        .base_url("http://127.0.0.1:1/v1")
        .build()?;
    assert!(matches!(
        client.evaluate("text", &Batch::new()).await,
        Err(Error::InvalidRequest(_))
    ));
    for state in [Value::Null, json!(42), json!(false)] {
        assert!(matches!(
            client.ask(&state, &Question::noul("question")).await,
            Err(Error::InvalidRequest(_))
        ));
    }
    Ok(())
}
