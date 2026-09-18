use super::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

fn payload() -> Value {
    let mut value = crate::strikes::tests::payload();
    value["member"]["permissions"] = json!(Permissions::ADMINISTRATOR.bits().to_string());
    value["data"]["name"] = json!("addaction");
    value["data"]["options"] = json!([
        {"name": "question", "type": 3, "value": "  Ban after 3 strikes  "}
    ]);
    value
}

fn interaction(value: Value) -> Interaction {
    serde_json::from_value(value).unwrap()
}

fn unused_jev() -> typesafe::Client {
    typesafe::Client::builder("unused-key")
        .base_url("http://127.0.0.1:1/v1")
        .max_retries(0)
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap()
}

fn role(id: u64, name: &str, managed: bool) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "managed": managed,
        "position": 1, "color": 0, "colors": {"primary_color": 0},
        "hoist": false, "mentionable": false, "permissions": "0", "flags": 0,
    }))
    .unwrap()
}

#[test]
fn role_names_and_mentions_resolve_only_existing_unmanaged_guild_roles() {
    let roles = vec![
        role(100, "@everyone", false),
        role(200, "Trusted Member", false),
        role(201, "Muted", false),
        role(202, "Integration", true),
        role(203, "Muted", false),
        role(204, "Équipe", false),
    ];
    for reference in [
        "Trusted Member",
        " trusted member ",
        "@Trusted Member",
        "<@&200>",
    ] {
        assert_eq!(resolve_role(100, reference, &roles).unwrap(), 200);
    }
    assert_eq!(resolve_role(100, "<@&201>", &roles).unwrap(), 201);
    assert_eq!(resolve_role(100, "éQUIPE", &roles).unwrap(), 204);
    for reference in [
        "Missing",
        "<@&999>",
        "<@200>",
        "<#200>",
        "<@&0>",
        "<@&invalid>",
        "Muted",
        "@everyone",
        "<@&100>",
        "Integration",
        "<@&202>",
    ] {
        let error = resolve_role(100, reference, &roles).unwrap_err();
        assert!(
            error.downcast_ref::<RoleRuleError>().is_some(),
            "{reference}"
        );
        assert!(error_message(&error).contains("No action was saved"));
    }
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn role_rules_save_and_reload_in_all_modes_and_reject_invalid_roles() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let guild = 7_600_000_000_000_000_i64 + i64::from(fastrand::u32(..));
    for (index, (kind, scoped)) in [
        ActionKind::MessageBinary,
        ActionKind::MessageCode,
        ActionKind::StrikeBinary,
        ActionKind::StrikeCode,
    ]
    .into_iter()
    .flat_map(|kind| [(kind, false), (kind, true)])
    .enumerate()
    {
        let action = NewAction {
            guild_id: guild,
            interaction_id: guild + index as i64,
            question: if scoped {
                "Revoke Trusted Member after spam in #general"
            } else {
                "Revoke Trusted Member after spam"
            }
            .into(),
        };
        let mut responses = Vec::new();
        if scoped {
            responses.push((200, choice_response(ActionKind::ContainsChannels)));
        }
        responses.extend([
            (200, choice_response(ActionKind::ContainsRoles)),
            (200, resolved_role_response(kind)),
        ]);
        let (jev, requests) = jev_responses(responses)?;
        let saved = save_with_services(
            &pool,
            &jev,
            &action,
            |mode, question| async move {
                assert_eq!(Some(mode), kind.code_mode());
                assert_eq!(question, "Revoke Trusted Member after spam");
                Ok("events => 'REVOKE_ROLE'".into())
            },
            |_, _| async {
                assert!(
                    scoped,
                    "rules without channel restrictions must not split channels"
                );
                Ok(ScopedRule {
                    question: "Revoke Trusted Member after spam".into(),
                    channels: vec![200],
                })
            },
            |guild_id, question| async move {
                assert_eq!(guild_id, guild);
                assert_eq!(question, "Revoke Trusted Member after spam");
                Ok(Some(resolve_role(
                    guild,
                    "Trusted Member",
                    &[role(300, "Trusted Member", false)],
                )?))
            },
        )
        .await?
        .unwrap();
        let requests = requests.join().unwrap();
        assert_eq!(requests.len(), if scoped { 3 } else { 2 });
        let classified = requests.last().unwrap();
        assert_eq!(classified["state"], "Revoke Trusted Member after spam");
        assert!(
            classified["questions"]["answer"]["criteria"]
                .get("contains_roles")
                .is_none()
        );
        assert!(
            requests[0]["questions"]["answer"]["criteria"]
                .get("contains_roles")
                .is_some()
        );
        assert_eq!(saved.kind, kind);
        assert_eq!(saved.role_id, Some(300));
        assert_eq!(saved.only_channels, scoped.then_some(vec![200]));
        let query = if kind.is_message() {
            "SELECT id, guild_id, question, code, only_channels, role_id FROM message_actions WHERE id = $1"
        } else {
            "SELECT id, guild_id, question, code, only_channels, role_id FROM strike_actions WHERE id = $1"
        };
        let stored: jeeves::message_actions::MessageAction = sqlx::query_as(query)
            .bind(saved.id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(stored.role_id, Some(300));
        assert_eq!(stored.code.is_some(), kind.code_mode().is_some());
        let retry = save_with_services(
            &pool,
            &unused_jev(),
            &action,
            |_, _| async { panic!("must not regenerate") },
            |_, _| async { panic!("must not split again") },
            |_, _| async { panic!("must keep saved role even if renamed") },
        )
        .await?;
        assert_eq!(retry, Some(saved));
    }
    let action = NewAction {
        guild_id: guild,
        interaction_id: guild + 10,
        question: "Give Missing for helpful messages".into(),
    };
    let (jev, request) = jev_response(200, choice_response(ActionKind::ContainsRoles))?;
    let error = save_with_services(
        &pool,
        &jev,
        &action,
        |_, _| async { panic!("must not generate") },
        |_, _| async { panic!("must not split") },
        |guild, _| async move { resolve_role(guild, "Missing", &[]).map(Some) },
    )
    .await
    .unwrap_err();
    request.join().unwrap();
    assert!(error_message(&error).contains("could not be found"));
    assert_eq!(existing(&pool, &action).await?, None);
    let (jev, request) = jev_response(200, choice_response(ActionKind::ContainsRoles))?;
    let error = save_with_services(
        &pool,
        &jev,
        &action,
        |_, _| async { panic!("must not generate without a role") },
        |_, _| async { panic!("must not split channels") },
        |_, _| async { Ok(None) },
    )
    .await
    .unwrap_err();
    request.join().unwrap();
    assert!(error_message(&error).contains("couldn't identify the role"));
    assert_eq!(existing(&pool, &action).await?, None);

    // A failed final classification must never save a partially resolved rule.
    for kind in [
        ActionKind::NoneOfTheAbove,
        ActionKind::ContainsChannels,
        ActionKind::ContainsRoles,
    ] {
        let (jev, requests) = jev_responses(vec![
            (200, choice_response(ActionKind::ContainsRoles)),
            (200, resolved_role_response(kind)),
        ])?;
        let result = save_with_services(
            &pool,
            &jev,
            &action,
            |_, _| async { panic!("must not generate unsupported rules") },
            |_, _| async { panic!("must not split channels") },
            |_, _| async { Ok(Some(300)) },
        )
        .await;
        if kind == ActionKind::NoneOfTheAbove {
            assert_eq!(result?, None);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(existing(&pool, &action).await?, None);
        requests.join().unwrap();
    }
    for query in [
        "DELETE FROM message_actions WHERE guild_id=$1",
        "DELETE FROM strike_actions WHERE guild_id=$1",
    ] {
        sqlx::query(query).bind(guild).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}

#[test]
fn accepts_only_question_and_validates_permissions_and_scope() {
    let options = options();
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].name, "question");
    assert_eq!(options[0].required, Some(true));
    let action = parse(&interaction(payload())).unwrap();
    assert_eq!(action.guild_id, 100);
    assert_eq!(action.question, "Ban after 3 strikes");
    for permission in [
        json!("0"),
        json!(Permissions::MODERATE_MEMBERS.bits().to_string()),
        Value::Null,
    ] {
        let mut value = payload();
        value["member"]["permissions"] = permission;
        assert!(
            parse(&interaction(value))
                .unwrap_err()
                .contains("Administrator")
        );
    }
    let mut value = payload();
    value["guild_id"] = Value::Null;
    assert!(parse(&interaction(value)).unwrap_err().contains("server"));
    for question in [" \n\t".into(), "x".repeat(1001)] {
        let mut value = payload();
        value["data"]["options"][0]["value"] = json!(question);
        assert!(parse(&interaction(value)).is_err());
    }
    let mut value = payload();
    value["data"]["options"][0]["value"] = json!("é".repeat(1000));
    assert!(parse(&interaction(value)).is_ok());
    let mut value = payload();
    value["data"]["options"] = json!([]);
    assert!(parse(&interaction(value)).is_err());
}

#[tokio::test]
async fn unauthorized_commands_never_reach_database_or_models() -> Result<()> {
    let pool = PgPool::connect_lazy("postgres://localhost/unused")?;
    let mut unauthorized = payload();
    unauthorized["member"]["permissions"] = json!("0");
    let response = handle_with_generator(
        &pool,
        &unused_jev(),
        &interaction(unauthorized),
        |_, _| async { panic!("invalid request must not reach Gemini") },
    )
    .await;
    assert!(response.contains("Administrator"));
    Ok(())
}

#[tokio::test]
async fn jev_classifies_with_key_description_choices() -> Result<()> {
    for kind in [
        ActionKind::MessageBinary,
        ActionKind::MessageCode,
        ActionKind::StrikeBinary,
        ActionKind::StrikeCode,
        ActionKind::ContainsChannels,
        ActionKind::ContainsRoles,
        ActionKind::NoneOfTheAbove,
    ] {
        let (jev, request) = jev_response(200, choice_response(kind))?;
        assert_eq!(classify(&jev, "Ban after 3 strikes", false).await?, kind);
        let request = request.join().unwrap();
        assert_eq!(request["state"], "Ban after 3 strikes");
        let question = &request["questions"]["answer"];
        assert_eq!(question["type"], "choice");
        let descriptions = question["criteria"].as_object().unwrap();
        assert_eq!(descriptions.len(), 7);
        assert!(
            descriptions["contains_channels"]
                .as_str()
                .unwrap()
                .contains("WHERE")
        );
        assert!(
            descriptions["contains_roles"]
                .as_str()
                .unwrap()
                .contains("OUTCOME")
        );
        for key in ["message_binary", "strike_binary"] {
            assert!(
                descriptions[key]
                    .as_str()
                    .unwrap()
                    .contains("no arithmetic or computation")
            );
        }
        for key in ["message_code", "strike_code"] {
            assert!(
                descriptions[key]
                    .as_str()
                    .unwrap()
                    .contains("arithmetic/computation")
            );
        }
        assert!(
            descriptions["none_of_the_above"]
                .as_str()
                .unwrap()
                .contains("unsupported")
        );
    }
    for (status, body) in [
        (503, json!({"error": "unavailable"})),
        (
            200,
            json!({"model": "jev-test", "usage": {}, "answers": {"answer": {
                "type": "choice", "choice": "unexpected", "probabilities": {"unexpected": 1}, "confidence": 1
            }}}),
        ),
    ] {
        let (jev, request) = jev_response(status, body)?;
        assert!(classify(&jev, "Ban after 3 strikes", false).await.is_err());
        request.join().unwrap();
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn classifies_saves_rejects_failures_and_deduplicates_across_types() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let guild = 7_000_000_000_000_000_i64 + i64::from(fastrand::u32(..));
    for (index, kind) in [
        ActionKind::MessageBinary,
        ActionKind::MessageCode,
        ActionKind::StrikeBinary,
        ActionKind::StrikeCode,
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = payload();
        value["guild_id"] = json!(guild.to_string());
        value["id"] = json!((guild + index as i64).to_string());
        let action = parse(&interaction(value)).unwrap();
        let (jev, request) = jev_response(200, choice_response(kind))?;
        let saved = save(&pool, &jev, &action, |mode, question| async move {
            assert_eq!(Some(mode), kind.code_mode());
            assert_eq!(question, "Ban after 3 strikes");
            Ok("events => events.length >= 3 ? 'BAN' : null".into())
        })
        .await?
        .unwrap();
        request.join().unwrap();
        assert_eq!(saved.kind, kind);
        let row: (i64, Option<Vec<i64>>, String, Option<String>) =
            sqlx::query_as(if kind.is_message() {
                "SELECT guild_id, only_channels, question, code FROM message_actions WHERE id = $1"
            } else {
                "SELECT guild_id, only_channels, question, code FROM strike_actions WHERE id = $1"
            })
            .bind(saved.id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            (row.0, row.1, row.2),
            (guild, None, action.question.clone())
        );
        assert_eq!(row.3.is_some(), kind.code_mode().is_some());
        assert_eq!(
            save(&pool, &unused_jev(), &action, |_, _| async {
                panic!("retry must not regenerate")
            })
            .await?,
            Some(saved)
        );
    }
    let mut value = payload();
    value["guild_id"] = json!(guild.to_string());
    value["id"] = json!((guild + 10).to_string());
    let interaction = interaction(value);
    let action = parse(&interaction).unwrap();
    for body in [
        choice_response(ActionKind::NoneOfTheAbove),
        json!({"invalid": "response"}),
    ] {
        let (jev, request) = jev_response(200, body)?;
        let response = handle_with_generator(&pool, &jev, &interaction, |_, _| async {
            panic!("unsupported or failed classification must not generate code")
        })
        .await;
        assert!(response.starts_with("Error:"));
        assert_eq!(existing(&pool, &action).await?, None);
        request.join().unwrap();
    }
    for code in ["not JavaScript", "42", "async events => null"] {
        let (jev, request) = jev_response(200, choice_response(ActionKind::MessageCode))?;
        assert!(
            save(&pool, &jev, &action, |_, _| async { Ok(code.into()) })
                .await
                .is_err()
        );
        assert_eq!(existing(&pool, &action).await?, None);
        request.join().unwrap();
    }
    let (jev, request) = jev_response(200, choice_response(ActionKind::StrikeCode))?;
    assert!(
        save(&pool, &jev, &action, |_, _| async {
            anyhow::bail!("Gemini unavailable")
        })
        .await
        .is_err()
    );
    assert_eq!(existing(&pool, &action).await?, None);
    request.join().unwrap();

    // Simultaneous retries can receive different classifications. Both must
    // report the one stored result, with no duplicate even across the two tables.
    let (message_jev, message_request) =
        jev_response(200, choice_response(ActionKind::MessageCode))?;
    let (strike_jev, strike_request) = jev_response(200, choice_response(ActionKind::StrikeCode))?;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let generator = |_, _| {
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            Ok("events => null".to_owned())
        }
    };
    let (first, second) = tokio::try_join!(
        save(&pool, &message_jev, &action, generator),
        save(&pool, &strike_jev, &action, generator),
    )?;
    assert!(first.is_some());
    assert_eq!(first, second);
    message_request.join().unwrap();
    strike_request.join().unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM message_actions WHERE created_by_interaction_id = $1) +
                (SELECT COUNT(*) FROM strike_actions WHERE created_by_interaction_id = $1)",
    )
    .bind(action.interaction_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);
    for query in [
        "DELETE FROM message_actions WHERE guild_id = $1",
        "DELETE FROM strike_actions WHERE guild_id = $1",
    ] {
        sqlx::query(query).bind(guild).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}

fn choice_response(kind: ActionKind) -> Value {
    let mut probabilities = json!({"message_binary": 0, "message_code": 0, "strike_binary": 0, "strike_code": 0, "contains_channels": 0, "contains_roles": 0, "none_of_the_above": 0});
    probabilities[kind.name()] = json!(1);
    json!({"model": "jev-test", "usage": {}, "answers": {"answer": {"type": "choice", "choice": kind, "probabilities": probabilities, "confidence": 1}}})
}

fn resolved_role_response(kind: ActionKind) -> Value {
    let mut response = choice_response(kind);
    response["answers"]["answer"]["probabilities"]
        .as_object_mut()
        .unwrap()
        .remove("contains_roles");
    response
}

fn jev_response(status: u16, body: Value) -> Result<(typesafe::Client, thread::JoinHandle<Value>)> {
    let (client, requests) = jev_responses(vec![(status, body)])?;
    Ok((
        client,
        thread::spawn(move || requests.join().unwrap().pop().unwrap()),
    ))
}

fn jev_responses(
    responses: Vec<(u16, Value)>,
) -> Result<(typesafe::Client, thread::JoinHandle<Vec<Value>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let client = typesafe::Client::builder("test-key")
        .base_url(format!("http://{}/v1", listener.local_addr()?))
        .timeout(Duration::from_secs(5))
        .max_retries(0)
        .build()?;
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
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
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            requests.push(request);
        }
        requests
    });
    Ok((client, task))
}

#[test]
fn channel_references_resolve_exactly_and_reject_unknown_or_ambiguous_names() {
    let channel = |id, name, kind| {
        serde_json::from_value::<Channel>(json!({"id": id, "name": name, "type": kind})).unwrap()
    };
    let channels = vec![
        channel("200", "general", 0),
        channel("201", "chat", 0),
        channel("202", "general", 0),
        channel("203", "category", 4),
    ];
    let split = |references: Vec<&str>| ChannelRule {
        statement: "ban spam".into(),
        channels: references.into_iter().map(str::to_owned).collect(),
    };
    let rule = resolve_channels(split(vec!["<#200>", "#chat", "<#200>"]), &channels).unwrap();
    assert_eq!(rule.channels, vec![200, 201]);
    assert_eq!(rule.question, "ban spam");
    for refs in [
        vec!["#general"],
        vec!["#unknown"],
        vec!["<#999>"],
        vec!["#category"],
        vec![],
    ] {
        assert!(resolve_channels(split(refs), &channels).is_err());
    }
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn channel_branch_extracts_reclassifies_saves_scope_and_deduplicates() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let guild = 7_500_000_000_000_000_i64 + i64::from(fastrand::u32(..));
    let action = |index| NewAction {
        guild_id: guild,
        interaction_id: guild + index,
        question: "Ban after 3 strikes in #general and #chat".into(),
    };
    for (index, kind) in [
        ActionKind::MessageBinary,
        ActionKind::MessageCode,
        ActionKind::StrikeBinary,
        ActionKind::StrikeCode,
    ]
    .into_iter()
    .enumerate()
    {
        let action = action(index as i64);
        let (jev, requests) = jev_responses(vec![
            (200, choice_response(ActionKind::ContainsChannels)),
            (200, choice_response(kind)),
        ])?;
        let saved = save_with_splitter(
            &pool,
            &jev,
            &action,
            |mode, question| async move {
                assert_eq!(Some(mode), kind.code_mode());
                assert_eq!(question, "Ban after 3 strikes");
                Ok("events => events.length >= 3 ? 'BAN' : null".into())
            },
            |guild_id, question| async move {
                assert_eq!(guild_id, guild);
                assert_eq!(question, "Ban after 3 strikes in #general and #chat");
                Ok(ScopedRule {
                    question: "Ban after 3 strikes".into(),
                    channels: vec![200, 201],
                })
            },
        )
        .await?
        .unwrap();
        let requests = requests.join().unwrap();
        assert_eq!(requests[0]["state"], action.question);
        assert_eq!(requests[1]["state"], "Ban after 3 strikes");
        assert_eq!(saved.kind, kind);
        assert_eq!(saved.only_channels, Some(vec![200, 201]));
        let query = if kind.is_message() {
            "SELECT question, only_channels FROM message_actions WHERE id=$1"
        } else {
            "SELECT question, only_channels FROM strike_actions WHERE id=$1"
        };
        let row: (String, Option<Vec<i64>>) = sqlx::query_as(query)
            .bind(saved.id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(row, ("Ban after 3 strikes".into(), Some(vec![200, 201])));
        let retry = save_with_splitter(
            &pool,
            &unused_jev(),
            &action,
            |_, _| async { panic!("must not regenerate") },
            |_, _| async { panic!("must not extract again") },
        )
        .await?;
        assert_eq!(retry, Some(saved));
    }
    let rejected = action(10);
    for kind in [ActionKind::ContainsChannels, ActionKind::NoneOfTheAbove] {
        let (jev, requests) = jev_responses(vec![
            (200, choice_response(ActionKind::ContainsChannels)),
            (200, choice_response(kind)),
        ])?;
        let result = save_with_splitter(
            &pool,
            &jev,
            &rejected,
            |_, _| async { panic!("must not generate unsupported rules") },
            |_, _| async {
                Ok(ScopedRule {
                    question: "Invalid rule".into(),
                    channels: vec![200],
                })
            },
        )
        .await;
        if kind == ActionKind::ContainsChannels {
            assert!(result.is_err());
        } else {
            assert_eq!(result?, None);
        }
        assert_eq!(existing(&pool, &rejected).await?, None);
        requests.join().unwrap();
    }
    let (jev, request) = jev_response(200, choice_response(ActionKind::ContainsChannels))?;
    assert!(
        save_with_splitter(
            &pool,
            &jev,
            &rejected,
            |_, _| async { panic!("must not generate after extraction failure") },
            |_, _| async { anyhow::bail!(ChannelRuleError("Unknown channel")) }
        )
        .await
        .is_err()
    );
    request.join().unwrap();
    assert_eq!(existing(&pool, &rejected).await?, None);
    for query in [
        "DELETE FROM message_actions WHERE guild_id=$1",
        "DELETE FROM strike_actions WHERE guild_id=$1",
    ] {
        sqlx::query(query).bind(guild).execute(&pool).await?;
    }
    pool.close().await;
    Ok(())
}
