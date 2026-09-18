use super::*;
use crate::message_actions::{ActionError, tests::jev_response};

#[tokio::test]
async fn strike_role_outcomes_use_the_configured_role_in_both_modes() -> Result<()> {
    for (level, code, expected) in [
        (
            2,
            "'GIVE_ROLE'",
            MessageActionOutcome::GiveRole {
                role_id: 9007199254740993,
                reason: "rule-0".into(),
            },
        ),
        (
            3,
            "'REVOKE_ROLE'",
            MessageActionOutcome::RevokeRole {
                role_id: 9007199254740993,
                reason: "rule-0".into(),
            },
        ),
        (4, "null", MessageActionOutcome::Ignore),
    ] {
        let (client, request) = jev_response(
            200,
            crate::message_actions::tests::role_score_response(true, level),
        )?;
        let mut input = context(&[None]);
        input.actions[0].role_id = Some(9007199254740993);
        let report = process_strike(input, client).await;
        assert_eq!(*report.results[0].result.as_ref().unwrap(), expected);
        let request = request.join().unwrap();
        assert_eq!(
            request["questions"]["answer"]["criteria"],
            json!(["ban", "kick", "give role", "revoke role", "no action"])
        );

        let code = format!("strikes => {code}");
        let mut input = context(&[Some(&code)]);
        input.actions[0].role_id = Some(9007199254740993);
        let client = typesafe::Client::builder("test-key")
            .base_url("http://127.0.0.1:1/v1")
            .build()?;
        let report = process_strike(input, client).await;
        assert_eq!(*report.results[0].result.as_ref().unwrap(), expected);
    }
    Ok(())
}

fn strike(id: i64) -> StoredStrike {
    StoredStrike {
        id,
        guild_id: 100,
        channel_id: 200,
        user_id: 300,
        moderator_id: 400,
        reason: format!("reason-{id}"),
        created_at: OffsetDateTime::UNIX_EPOCH,
        interaction_id: Some(500 + id),
        source_message_id: None,
        source_action_id: None,
    }
}

fn context(codes: &[Option<&str>]) -> StrikeContext {
    StrikeContext {
        strike: strike(3),
        history: vec![strike(1), strike(2)],
        actions: codes
            .iter()
            .enumerate()
            .map(|(id, code)| StrikeAction {
                id: id as i32,
                guild_id: 100,
                only_channels: None,
                role_id: None,
                question: format!("rule-{id}"),
                code: code.map(str::to_owned),
            })
            .collect(),
    }
}

#[tokio::test]
async fn strike_code_sees_the_members_history_and_reconciles_all_rules() -> Result<()> {
    let context = context(&[
        Some("strikes => strikes.length >= 3 ? 'BAN' : null"),
        Some("strikes => 'KICK'"),
        Some(
            r#"strikes => {
            if (strikes[0].id !== "1" || strikes.at(-1).id !== "3" ||
                strikes.at(-1).user_id !== "300" || strikes.at(-1).reason !== "reason-3" ||
                strikes.at(-1).created_at !== 0) throw new Error("invalid strike input");
            return null;
        }"#,
        ),
        Some("strikes => 'STRIKE'"),
        Some("strikes => { throw new Error('broken rule'); }"),
    ]);
    let client = typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1/v1")
        .build()?;
    let report = process_strike(context, client).await;
    assert_eq!(report.strike_id, 3);
    assert_eq!(
        *report.results[0].result.as_ref().unwrap(),
        MessageActionOutcome::Ban("rule-0".into())
    );
    assert_eq!(
        *report.results[1].result.as_ref().unwrap(),
        MessageActionOutcome::Kick("rule-1".into())
    );
    assert_eq!(
        *report.results[2].result.as_ref().unwrap(),
        MessageActionOutcome::Ignore
    );
    assert!(matches!(
        report.results[3].result,
        Err(ActionError::Handler(_))
    ));
    assert!(matches!(
        report.results[4].result,
        Err(ActionError::Handler(_))
    ));
    Ok(())
}

#[tokio::test]
async fn strike_questions_send_the_history_and_map_score_levels() -> Result<()> {
    for (level, expected) in [
        (0, MessageActionOutcome::Ban("rule-0".into())),
        (1, MessageActionOutcome::Kick("rule-0".into())),
        (2, MessageActionOutcome::Ignore),
    ] {
        let (client, request) = jev_response(
            200,
            json!({
                "model": "jev-test", "usage": {},
                "answers": {"answer": {
                    "type": "score", "score": level, "confidence": 1,
                    "legend": {"0":"ban", "1":"kick", "2":"no action"},
                    "probabilities": {
                        "0": if level == 0 { 1 } else { 0 },
                        "1": if level == 1 { 1 } else { 0 },
                        "2": if level == 2 { 1 } else { 0 },
                    },
                }},
            }),
        )?;
        let report = process_strike(context(&[None]), client).await;
        assert_eq!(*report.results[0].result.as_ref().unwrap(), expected);
        let request = request.join().unwrap();
        assert_eq!(
            request["questions"]["answer"]["criteria"],
            json!(["ban", "kick", "no action"])
        );
        assert!(
            request["questions"]["answer"]["instructions"]
                .as_str()
                .unwrap()
                .ends_with("rule-0")
        );
        assert_eq!(request["state"]["total_strikes"], 3);
        assert_eq!(request["state"]["current_strike"]["id"], "3");
        assert_eq!(request["state"]["history"].as_array().unwrap().len(), 2);
    }
    Ok(())
}
