use super::*;
use commands::{Command, parse};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub(super) path: String,
    pub(super) body: Value,
    pub(super) headers: String,
}

pub(super) fn server(responses: Vec<(u16, Value)>) -> (String, thread::JoinHandle<Vec<Request>>) {
    let (base, task, _) = observed_server(responses);
    (base, task)
}

pub(super) fn observed_server(
    responses: Vec<(u16, Value)>,
) -> (
    String,
    thread::JoinHandle<Vec<Request>>,
    mpsc::UnboundedReceiver<Request>,
) {
    let (observed, receiver) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut connection = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "missing HTTP request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            // On macOS accepted sockets inherit the listener's nonblocking flag.
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let size = connection.read(&mut chunk).unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let size = connection.read(&mut chunk).unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            requests.push(Request {
                path: headers.lines().next().unwrap().into(),
                body: if length == 0 {
                    Value::Null
                } else if headers
                    .to_ascii_lowercase()
                    .contains("application/x-www-form-urlencoded")
                {
                    let form: std::collections::BTreeMap<String, String> =
                        url::form_urlencoded::parse(&bytes[header_end..header_end + length])
                            .into_owned()
                            .collect();
                    serde_json::to_value(form).unwrap()
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                },
                headers,
            });
            let body = body.to_string();
            write!(connection, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            let _ = observed.send(requests.last().unwrap().clone());
        }
        requests
    });
    (base, task, receiver)
}

pub(super) fn message() -> ChatMessage {
    serde_json::from_value(json!({
        "broadcaster_user_id": "100", "chatter_user_id": "200", "chatter_user_login": "viewer",
        "message_id": "uuid-message", "message": {"text": "hello"}, "badges": [],
        "source_broadcaster_user_id": null,
    }))
    .unwrap()
}

fn classification(choice: &str) -> Value {
    let mut probabilities = json!({"message": 0.0, "strike_threshold": 0.0, "unsupported": 0.0});
    probabilities[choice] = json!(1.0);
    json!({"model": "jev-test", "usage": {}, "answers": {"answer": {
        "type": "choice", "choice": choice, "probabilities": probabilities, "confidence": 1.0,
    }}})
}

fn extraction(value: Value) -> Value {
    json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": value.to_string()}]}}]})
}

#[tokio::test]
async fn enrollment_checks_all_moderator_pages_and_unsubscribe_is_idempotent() -> Result<()> {
    let (base, requests) = server(vec![
        (
            200,
            json!({"data": [{"broadcaster_id": "elsewhere"}], "pagination": {"cursor": "next page"}}),
        ),
        (
            200,
            json!({"data": [{"broadcaster_id": "200"}], "pagination": {}}),
        ),
        (404, json!({})),
    ]);
    let api = api::Api::for_test(base);
    assert!(api.moderates("200").await?);
    api.unsubscribe("already-removed").await?;
    let requests = requests.join().unwrap();
    assert!(requests[0].path.contains("user_id=300"));
    assert!(requests[0].path.contains("first=100"));
    assert!(requests[1].path.contains("after=next+page"));
    assert!(
        requests[2]
            .path
            .starts_with("DELETE /eventsub/subscriptions?id=already-removed")
    );
    Ok(())
}

#[tokio::test]
async fn ordinary_lobby_messages_do_not_reach_storage_or_models() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
    let api = api::Api::for_test("http://127.0.0.1:1".into());
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    let event = json!({"metadata": {"message_id": "lobby-event", "message_timestamp": "2026-09-18T00:00:00Z"},
    "payload": {"subscription": {"type": "channel.chat.message"}, "event": {
        "broadcaster_user_id": "300", "chatter_user_id": "200", "chatter_user_login": "viewer",
        "message_id": "lobby-message", "badges": [], "message": {"text": "hello"},
    }}});
    handle_event(
        &pool,
        &api,
        &jev,
        &gemini,
        serde_json::from_value(event)?,
        &mut None,
    )
    .await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn channel_refresh_starts_workers_and_cancels_removed_workers() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    let mut api = api::Api::for_test("http://127.0.0.1:1".into());
    api.user_id = format!("worker-bot-{}", std::process::id());
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
        .bind(&api.user_id)
        .execute(&pool)
        .await?;
    let mut workers = ChannelWorkers {
        tasks: JoinSet::new(),
        channels: HashMap::new(),
    };
    let lobby = workers.refresh(&pool, &api, &jev, &gemini).await?.unwrap();
    assert_eq!(lobby.len(), 1);
    assert!(lobby.contains_key(&api.user_id));
    sqlx::query(
        "INSERT INTO twitch_channels (bot_user_id, channel_id, login) VALUES ($1, '100', 'viewer')",
    )
    .bind(&api.user_id)
    .execute(&pool)
    .await?;
    let joined = workers.refresh(&pool, &api, &jev, &gemini).await?.unwrap();
    let removed_sender = joined["100"].clone();
    let removed_task = workers.channels["100"].1.id();
    assert_eq!(joined.len(), 2);
    assert!(workers.refresh(&pool, &api, &jev, &gemini).await?.is_none());
    sqlx::query("DELETE FROM twitch_channels WHERE bot_user_id = $1")
        .bind(&api.user_id)
        .execute(&pool)
        .await?;
    let left = workers.refresh(&pool, &api, &jev, &gemini).await?.unwrap();
    assert_eq!(left.len(), 1);
    let error = timeout(Duration::from_secs(2), workers.tasks.join_next())
        .await?
        .unwrap()
        .unwrap_err();
    assert!(error.is_cancelled());
    assert_eq!(error.id(), removed_task);
    assert!(removed_sender.is_closed());
    assert!(!workers.channels[&api.user_id].1.is_finished());
    workers.tasks.shutdown().await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn addaction_authorization_precedes_database_and_models() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    let (base, replies) = server(vec![(200, json!({"data": [{"is_sent": true}]}))]);
    commands::handle(
        &pool,
        &api::Api::for_test(base),
        &jev,
        &gemini,
        &message(),
        Ok(Command::AddAction("Ban users after three strikes".into())),
    )
    .await?;
    assert!(
        replies.join().unwrap()[0].body["message"]
            .as_str()
            .unwrap()
            .contains("Only this channel's broadcaster")
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn addaction_classifies_extracts_saves_and_rejects_failed_rules() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    let channel = format!("twitch-addaction-{}", std::process::id());
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    let (base, jev_requests) = server(vec![
        (200, classification("message")),
        (200, classification("strike_threshold")),
        (200, classification("unsupported")),
        (200, classification("message")),
        (503, json!({"error": "unavailable"})),
    ]);
    let jev = typesafe::Client::builder("test-key")
        .base_url(base)
        .max_retries(0)
        .build()?;
    let (base, gemini_requests) = server(vec![
        (
            200,
            extraction(
                json!({"condition": "unsolicited advertising", "action": "strike", "timeout_seconds": null, "strike_threshold": null, "error": null}),
            ),
        ),
        (
            200,
            extraction(
                json!({"condition": "three strikes", "action": "timeout", "timeout_seconds": 600, "strike_threshold": 3, "error": null}),
            ),
        ),
        (
            200,
            extraction(
                json!({"condition": "spam", "action": "timeout", "timeout_seconds": null, "strike_threshold": null, "error": null}),
            ),
        ),
    ]);
    let gemini = crate::gemini::Gemini::for_test(base);
    let (base, replies) = server(vec![(200, json!({"data": [{"is_sent": true}]})); 5]);
    let api = api::Api::for_test(base);
    for (index, text) in [
        "!addaction Strike users who post unsolicited advertising.",
        "!jeeves addaction Time out users for ten minutes after three strikes.",
        "!addaction Give users the VIP role for helpful messages.",
        "!addaction Time out users for spam.",
        "!addaction Ban users who threaten violence.",
    ]
    .into_iter()
    .enumerate()
    {
        let event = json!({
            "metadata": {"message_id": format!("event-{index}"), "message_timestamp": "2026-09-18T00:00:00Z"},
            "payload": {"subscription": {"type": "channel.chat.message"}, "event": {
                "broadcaster_user_id": channel, "chatter_user_id": "200", "chatter_user_login": "moderator",
                "message_id": format!("command-{index}"), "badges": [{"set_id": "moderator"}], "message": {"text": text},
            }},
        });
        handle_event(
            &pool,
            &api,
            &jev,
            &gemini,
            serde_json::from_value(event.clone())?,
            &mut None,
        )
        .await?;
        // Redelivery must not call either model again or duplicate the confirmation/rule.
        handle_event(
            &pool,
            &api,
            &jev,
            &gemini,
            serde_json::from_value(event)?,
            &mut None,
        )
        .await?;
    }
    let rules = store::rules(&pool, &channel).await?;
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].condition, "unsolicited advertising");
    assert_eq!(rules[0].outcome()?, Action::Strike);
    assert!(rules[0].strike_threshold.is_none());
    assert_eq!(rules[1].outcome()?, Action::Timeout(600));
    assert_eq!(rules[1].strike_threshold, Some(3));
    assert_eq!(rules[1].condition, "At least 3 active strikes");
    let requests = jev_requests.join().unwrap();
    assert_eq!(
        requests[0].body["state"],
        "Strike users who post unsolicited advertising."
    );
    assert_eq!(requests[0].body["questions"]["answer"]["type"], "choice");
    let requests = gemini_requests.join().unwrap();
    assert!(
        requests[0].body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("trigger is message")
    );
    assert!(
        requests[1].body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("trigger is strike_threshold")
    );
    assert_eq!(
        requests[0].body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    let replies = replies.join().unwrap();
    assert!(
        replies[1].body["message"]
            .as_str()
            .unwrap()
            .contains("at least 3 active strikes")
    );
    for reply in &replies[2..] {
        assert!(
            reply.body["message"]
                .as_str()
                .unwrap()
                .contains("No action was saved.")
        );
    }
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}

#[test]
fn moderator_strike_commands_validate_user_and_cursor() {
    assert!(parse("!managestrikesx @viewer").is_none());
    for prefix in ["!managestrikes", "!jeeves managestrikes"] {
        for (input, after) in [("@Some_User", 0), ("Some_User 12", 12)] {
            let command = parse(&format!("{prefix} {input}")).unwrap().unwrap();
            assert_eq!(
                command,
                Command::ManageStrikes {
                    login: "some_user".into(),
                    after
                }
            );
            assert!(command.requires_moderator());
        }
        for input in [
            "",
            "@",
            "@@viewer",
            "bad-name",
            "🦀",
            "viewer -1",
            "viewer 0",
            "viewer 1 extra",
            "viewer 9223372036854775808",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(
                parse(&format!("{prefix} {input}")).unwrap().is_err(),
                "{input}"
            );
        }
    }
}

#[tokio::test]
async fn strike_lookup_requires_moderator_before_user_lookup_or_database() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    // The only permitted HTTP request is the denial sent back to chat.
    let (base, requests) = server(vec![(200, json!({"data": [{"is_sent": true}]}))]);
    commands::handle(
        &pool,
        &api::Api::for_test(base),
        &jev,
        &gemini,
        &message(),
        parse("!managestrikes @someone 10").unwrap(),
    )
    .await?;
    let requests = requests.join().unwrap();
    assert!(requests[0].path.starts_with("POST /chat/messages"));
    assert!(
        requests[0].body["message"]
            .as_str()
            .unwrap()
            .contains("Only this channel's broadcaster")
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn twitch_user_lookup_handles_missing_and_malformed_users() -> Result<()> {
    let (base, requests) = server(vec![
        (200, json!({"data": [{"id": "123", "login": "viewer"}]})),
        (200, json!({"data": []})),
        (200, json!({"data": [{}]})),
    ]);
    let api = api::Api::for_test(base);
    assert_eq!(api.user_id_for_login("viewer").await?, Some("123".into()));
    assert_eq!(api.user_id_for_login("missing").await?, None);
    assert!(api.user_id_for_login("broken").await.is_err());
    let requests = requests.join().unwrap();
    assert_eq!(requests[0].path, "GET /users?login=viewer HTTP/1.1");
    Ok(())
}

#[tokio::test]
async fn moderator_strike_lookup_reports_unknown_user_without_database() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://127.0.0.1:1/test")?;
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    let (base, requests) = server(vec![
        (200, json!({"data": []})),
        (200, json!({"data": [{"is_sent": true}]})),
    ]);
    let mut moderator = message();
    moderator.badges.push(Badge {
        set_id: "moderator".into(),
    });
    commands::handle(
        &pool,
        &api::Api::for_test(base),
        &jev,
        &gemini,
        &moderator,
        parse("!managestrikes @Missing").unwrap(),
    )
    .await?;
    let requests = requests.join().unwrap();
    assert_eq!(requests[0].path, "GET /users?login=missing HTTP/1.1");
    assert_eq!(
        requests[1].body["message"],
        "Twitch user @missing was not found."
    );
    pool.close().await;
    Ok(())
}

#[test]
fn commands_are_validated() {
    assert!(parse("hello !jeeves").is_none());
    assert!(parse("!jeevesx help").is_none());
    assert!(parse("!addactionx spam").is_none());
    for prefix in ["!addaction", "!jeeves addaction"] {
        let command = parse(&format!("{prefix} Strike users for spam."))
            .unwrap()
            .unwrap();
        assert_eq!(command, Command::AddAction("Strike users for spam.".into()));
        assert!(command.requires_moderator());
        assert!(parse(prefix).unwrap().is_err());
        assert!(
            parse(&format!("{prefix} {}", "🦀".repeat(1000)))
                .unwrap()
                .is_ok()
        );
        assert!(
            parse(&format!("{prefix} {}", "🦀".repeat(1001)))
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(
        parse("!jeeves add strike unsolicited advertising")
            .unwrap()
            .unwrap(),
        Command::Add {
            action: Action::Strike,
            condition: "unsolicited advertising".into()
        }
    );
    assert_eq!(
        parse("!jeeves escalate 3 timeout 600").unwrap().unwrap(),
        Command::Escalate {
            count: 3,
            action: Action::Timeout(600)
        }
    );
    assert_eq!(
        parse("!jeeves strikes 12").unwrap().unwrap(),
        Command::Strikes(12)
    );
    for command in [
        "add strike",
        "add timeout 0 spam",
        "add timeout 1209601 spam",
        "escalate 0 ban",
        "escalate 3 strike",
        "escalate 3 ban and kick",
        "remove -1",
        "strikes 1 extra",
    ] {
        assert!(
            parse(&format!("!jeeves {command}")).unwrap().is_err(),
            "{command}"
        );
    }
    assert!(!Command::Strikes(0).requires_moderator());
    assert!(Command::Forgive(1).requires_moderator());
    assert!(Command::Remove(1).requires_moderator());
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn moderator_strike_history_pages_and_removes_only_current_channel_records() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    let channel = format!("twitch-managestrikes-{}", std::process::id());
    let other_channel = format!("{channel}-other");
    let channels = vec![channel.clone(), other_channel.clone()];
    sqlx::query("DELETE FROM twitch_strikes WHERE channel_id = ANY($1)")
        .bind(&channels)
        .execute(&pool)
        .await?;
    let long_reason = "🦀".repeat(1000);
    let mut ids = Vec::new();
    for (index, (scope, user, reason, removed)) in [
        (channel.as_str(), "900", long_reason.as_str(), false),
        (channel.as_str(), "900", "Second violation", false),
        (
            channel.as_str(),
            "200",
            "Other user's private record",
            false,
        ),
        (
            other_channel.as_str(),
            "900",
            "Other channel's record",
            false,
        ),
        (channel.as_str(), "900", "Already forgiven", true),
    ]
    .into_iter()
    .enumerate()
    {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO twitch_strikes (channel_id, user_id, reason, source_message_id, source_rule_id, removed_at)
             VALUES ($1, $2, $3, $4, 1, CASE WHEN $5 THEN now() ELSE NULL END) RETURNING id"
        ).bind(scope).bind(user).bind(reason).bind(format!("fixture-{index}")).bind(removed)
            .fetch_one(&pool).await?;
        ids.push(id);
    }
    let sent = json!({"data": [{"is_sent": true}]});
    let user = json!({"data": [{"id": "900", "login": "target"}]});
    let (base, requests) = server(vec![
        (200, user.clone()),
        (200, sent.clone()),
        (200, user.clone()),
        (200, sent.clone()),
        (200, sent.clone()), // Viewer denied before lookup.
        (200, sent.clone()), // Moderator removes the first strike.
        (200, user),
        (200, sent),
    ]);
    let api = api::Api::for_test(base);
    let jev = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1")
        .max_retries(0)
        .build()?;
    let gemini = crate::gemini::Gemini::for_test("http://127.0.0.1:1".into());
    let mut viewer = message();
    viewer.broadcaster_user_id = channel.clone();
    for (text, moderator, broadcaster) in [
        ("!managestrikes @Target".to_owned(), true, false),
        (format!("!managestrikes @target {}", ids[0]), false, true),
        (format!("!managestrikes @target {}", ids[0]), false, false),
        (format!("!jeeves forgive {}", ids[0]), true, false),
        ("!jeeves managestrikes target".to_owned(), true, false),
    ] {
        viewer.badges = if moderator {
            vec![Badge {
                set_id: "moderator".into(),
            }]
        } else {
            vec![]
        };
        viewer.chatter_user_id = if broadcaster {
            channel.clone()
        } else {
            "200".into()
        };
        commands::handle(&pool, &api, &jev, &gemini, &viewer, parse(&text).unwrap()).await?;
    }
    let requests = requests.join().unwrap();
    let replies: Vec<_> = requests
        .iter()
        .filter(|request| request.path.starts_with("POST /chat/messages"))
        .map(|request| {
            assert_eq!(request.body["broadcaster_id"], channel);
            request.body["message"].as_str().unwrap()
        })
        .collect();
    assert_eq!(replies.len(), 5);
    assert!(replies[0].starts_with(&format!("@target strikes #{}:", ids[0])));
    assert!(replies[0].contains(&format!("Next: !managestrikes @target {}", ids[0])));
    assert!(replies[0].ends_with(&format!("Remove: !jeeves forgive {}", ids[0])));
    assert!(replies[1].contains("Second violation"));
    assert!(!replies[1].contains("Next:"));
    assert!(replies[2].contains("Only this channel's broadcaster"));
    assert!(replies[3].contains(&format!("Removed strike {}", ids[0])));
    assert!(replies[4].contains("Second violation"));
    for reply in replies {
        assert!(reply.chars().count() <= 500);
        assert!(
            !reply.contains("Other user's")
                && !reply.contains("Other channel's")
                && !reply.contains("Already forgiven")
        );
    }
    assert_eq!(
        store::strikes(&pool, &other_channel, "900", 0).await?.len(),
        1
    );
    assert_eq!(store::strikes(&pool, &channel, "200", 0).await?.len(), 1);
    sqlx::query("DELETE FROM twitch_strikes WHERE channel_id = ANY($1)")
        .bind(&channels)
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

#[test]
fn staff_bot_and_shared_chat_are_protected() {
    let mut message = message();
    assert!(!message.protected("300"));
    assert!(message.protected("200"));
    message.badges.push(Badge {
        set_id: "moderator".into(),
    });
    assert!(message.is_moderator());
    message.badges.clear();
    message.chatter_user_id = "100".into();
    assert!(message.is_moderator());
    message.source_broadcaster_user_id = Some("999".into());
    assert!(!message.is_local());
}

#[test]
fn reconciliation_keeps_strike_deletion_and_strongest_removal() {
    let actions = [
        (Action::Strike, "a"),
        (Action::Timeout(60), "b"),
        (Action::Timeout(600), "c"),
    ];
    assert_eq!(
        reconcile(&actions),
        (true, Some((&Action::Timeout(600), "c")))
    );
    let mut actions = actions.to_vec();
    actions.push((Action::Ban, "d"));
    assert_eq!(reconcile(&actions), (true, Some((&Action::Ban, "d"))));
    assert_eq!(reconcile(&[]), (false, None));
}

#[test]
fn jev_context_preserves_current_message_and_bounded_newest_history() {
    let mut message = message();
    message.message.text = "quoted \"text\" 🦀".repeat(100);
    let history = (0..500)
        .map(|id| json!({"id": id.to_string(), "content": "🦀".repeat(200)}))
        .collect::<Vec<_>>();
    let question = question("unsolicited advertising").unwrap();
    let state = state(&message, OffsetDateTime::UNIX_EPOCH, &history, &question).unwrap();
    assert_eq!(state["current_message"]["content"], message.message.text);
    assert_eq!(state["history"][0]["id"], "0");
    assert!(!state["history"].as_array().unwrap().is_empty());
    assert!(state["history"].as_array().unwrap().len() < 500);
    assert!(
        serde_json::to_vec(&state).unwrap().len() + question.json_size().unwrap() + 1024 <= 32_000
    );
}

#[test]
fn twitch_tokens_and_native_action_payloads_are_checked() {
    let mut identity = api::Identity {
        login: "bot".into(),
        client_id: "client".into(),
        user_id: "bot".into(),
        scopes: api::SCOPES.map(str::to_owned).to_vec(),
    };
    assert!(identity.check("client", Some("bot")).is_ok());
    assert!(identity.check("other", None).is_err());
    assert!(identity.check("client", Some("other")).is_err());
    identity.scopes.pop();
    assert!(identity.check("client", None).is_err());
    assert_eq!(
        api::ban_body("200", &Action::Timeout(60), "spam").unwrap(),
        json!({"data": {"user_id": "200", "duration": 60, "reason": "spam"}})
    );
    assert!(
        api::ban_body("200", &Action::Ban, "spam").unwrap()["data"]
            .get("duration")
            .is_none()
    );
    assert!(api::ban_body("200", &Action::Timeout(0), "spam").is_err());
}

#[tokio::test]
async fn twitch_http_uses_bot_identity_and_message_specific_deletion() -> Result<()> {
    let (base, requests) = server(vec![
        (202, json!({"data": [{"id": "subscription"}]})),
        (200, json!({})),
        (200, json!({})),
        (200, json!({"data": [{"is_sent": false}]})),
    ]);
    let api = api::Api::for_test(base);
    api.subscribe("session", "100", "channel.chat.message")
        .await?;
    api.apply(&message(), &Action::Delete, "spam").await?;
    api.apply(&message(), &Action::Timeout(600), "spam").await?;
    assert!(api.say("100", "hello").await.is_err());
    let requests = requests.join().unwrap();
    assert_eq!(
        requests[0].body["condition"],
        json!({"broadcaster_user_id": "100", "user_id": "300"})
    );
    assert_eq!(
        requests[0].body["transport"],
        json!({"method": "websocket", "session_id": "session"})
    );
    assert!(requests[1].path.starts_with("DELETE /moderation/chat?"));
    assert!(requests[1].path.contains("message_id=uuid-message"));
    assert!(requests[1].path.contains("moderator_id=300"));
    assert_eq!(requests[2].body["data"]["duration"], 600);
    assert_eq!(requests[3].body["sender_id"], "300");
    for request in requests {
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("client-id: client")
        );
        assert!(request.headers.contains("Bearer test-token"));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn twitch_strike_escalates_despite_failed_deletion_and_redelivery_does_nothing() -> Result<()>
{
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    let channel = format!("twitch-flow-{}", std::process::id());
    let mut message = message();
    message.broadcaster_user_id = channel.clone();
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_strikes WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
        "DELETE FROM twitch_messages WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    store::add_rule(&pool, &message, "spam", &Action::Strike, None).await?;
    message.message_id = "escalation-rule".into();
    store::add_rule(&pool, &message, "At least 1 strike", &Action::Ban, Some(1)).await?;
    let (base, requests) = server(vec![
        (404, json!({})),
        (200, json!({})),
        (200, json!({"data": [{"is_sent": true}]})),
    ]);
    let api = api::Api::for_test(base);
    let (base, jev_requests) = server(vec![(
        200,
        json!({
            "model": "jev-test", "usage": {}, "answers": {"answer": {
                "type": "choice", "choice": "match", "probabilities": {"match": 1.0, "ignore": 0.0}, "confidence": 1.0,
            }},
        }),
    )]);
    let jev = typesafe::Client::builder("test-key")
        .base_url(base)
        .max_retries(0)
        .build()?;
    let notification = json!({
        "metadata": {"message_id": "event", "message_timestamp": "2026-09-18T00:00:00Z"},
        "payload": {"subscription": {"type": "channel.chat.message"}, "event": {
            "broadcaster_user_id": channel, "chatter_user_id": "200", "chatter_user_login": "viewer",
            "message_id": "offending-message", "badges": [],
            // Unauthorized command text must not bypass the moderation pipeline.
            "message": {"text": "!jeeves add ban spam"},
        }},
    });
    handle_event(
        &pool,
        &api,
        &jev,
        &crate::gemini::Gemini::for_test("http://127.0.0.1:1".into()),
        serde_json::from_value(notification.clone())?,
        &mut None,
    )
    .await?;
    handle_event(
        &pool,
        &api,
        &jev,
        &crate::gemini::Gemini::for_test("http://127.0.0.1:1".into()),
        serde_json::from_value(notification)?,
        &mut None,
    )
    .await?;
    assert_eq!(store::strikes(&pool, &channel, "200", 0).await?.len(), 1);
    assert_eq!(store::rules(&pool, &channel).await?.len(), 2);
    let requests = requests.join().unwrap();
    assert!(requests[0].path.starts_with("DELETE "));
    assert!(requests[1].path.starts_with("POST /moderation/bans?"));
    assert!(requests[1].body["data"].get("duration").is_none());
    assert!(
        requests[2].body["message"]
            .as_str()
            .unwrap()
            .contains("1 active")
    );
    let requests = jev_requests.join().unwrap();
    assert_eq!(
        requests[0].body["state"]["current_message"]["author_id"],
        "200"
    );
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_strikes WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
        "DELETE FROM twitch_messages WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn twitch_storage_is_isolated_retained_and_strikes_are_idempotent() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    let mut message = message();
    message.broadcaster_user_id = format!("twitch-test-{}", std::process::id());
    let channel = message.broadcaster_user_id.clone();
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_messages WHERE channel_id = $1",
        "DELETE FROM twitch_strikes WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    let id = store::add_rule(&pool, &message, "spam", &Action::Strike, None).await?;
    assert_eq!(
        store::add_rule(&pool, &message, "spam", &Action::Strike, None).await?,
        id
    );
    let rules = store::rules(&pool, &channel).await?;
    assert_eq!(rules.len(), 1);
    assert!(store::rules(&pool, "different-channel").await?.is_empty());
    assert!(store::claim(&pool, &channel, "delivery").await?);
    assert!(!store::claim(&pool, &channel, "delivery").await?);
    let strike_rules = [&rules[0]];
    let (first, second) = tokio::join!(
        store::record_strikes(&pool, &message, &strike_rules),
        store::record_strikes(&pool, &message, &strike_rules)
    );
    assert_eq!(first?.0 + second?.0, 1);
    let strikes = store::strikes(&pool, &channel, &message.chatter_user_id, 0).await?;
    assert_eq!(strikes.len(), 1);
    assert!(!store::remove_strike(&pool, "different-channel", strikes[0].0).await?);
    assert!(store::remove_strike(&pool, &channel, strikes[0].0).await?);
    assert_eq!(
        store::record_strikes(&pool, &message, &[&rules[0]]).await?,
        (0, 0)
    );
    for index in 0..505 {
        message.message_id = format!("message-{index:04}");
        store::save_message(
            &pool,
            &message,
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(index),
        )
        .await?;
    }
    let history = store::history(&pool, &channel).await?;
    assert_eq!(history.len(), 500);
    assert_eq!(history[0]["id"], "message-0504");
    assert_eq!(history[499]["id"], "message-0005");
    store::clear_messages(
        &pool,
        &channel,
        Some("message-0504"),
        None,
        OffsetDateTime::now_utc(),
    )
    .await?;
    assert_eq!(store::history(&pool, &channel).await?.len(), 499);
    assert!(!store::claim(&pool, &channel, "delivery").await?);
    store::clear_messages(
        &pool,
        &channel,
        None,
        Some(&message.chatter_user_id),
        OffsetDateTime::now_utc(),
    )
    .await?;
    assert!(store::history(&pool, &channel).await?.is_empty());
    for query in [
        "DELETE FROM twitch_rules WHERE channel_id = $1",
        "DELETE FROM twitch_messages WHERE channel_id = $1",
        "DELETE FROM twitch_strikes WHERE channel_id = $1",
        "DELETE FROM twitch_receipts WHERE channel_id = $1",
    ] {
        sqlx::query(query).bind(&channel).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}
