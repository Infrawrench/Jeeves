use super::*;
use serde_json::{Value, json};

pub(crate) fn payload() -> Value {
    json!({
        "id": "500", "application_id": "600", "type": 2, "token": "test-token",
        "guild_id": "100", "channel": {"id": "200", "type": 0}, "entitlements": [],
        "authorizing_integration_owners": {"0": "100"},
        "member": {
            "user": {"id": "400", "username": "moderator", "discriminator": "0"},
            "roles": [], "deaf": false, "mute": false, "flags": 0,
            "permissions": Permissions::MODERATE_MEMBERS.bits().to_string()
        },
        "data": {
            "id": "700", "name": "strike", "type": 1,
            "options": [
                {"name": "user", "type": 6, "value": "300"},
                {"name": "reason", "type": 3, "value": "  Repeated spam  "}
            ],
            "resolved": {
                "users": {"300": {"id": "300", "username": "recipient", "discriminator": "0"}},
                "members": {"300": {"roles": [], "flags": 0, "pending": false, "permissions": "0"}}
            }
        }
    })
}

fn interaction(value: Value) -> Interaction {
    serde_json::from_value(value).unwrap()
}

#[test]
fn moderators_and_administrators_can_issue_strikes() {
    for permission in [Permissions::MODERATE_MEMBERS, Permissions::ADMINISTRATOR] {
        let mut value = payload();
        value["member"]["permissions"] = json!(permission.bits().to_string());
        let strike = parse(&interaction(value)).unwrap();
        assert_eq!(
            (
                strike.guild_id,
                strike.channel_id,
                strike.user_id,
                strike.moderator_id,
                strike.source
            ),
            (100, 200, 300, 400, StrikeSource::Interaction(500))
        );
        assert_eq!(strike.reason, "Repeated spam");
    }
}

#[test]
fn missing_or_insufficient_permissions_are_rejected() {
    for permission in [
        json!("0"),
        json!(Permissions::MANAGE_MESSAGES.bits().to_string()),
        Value::Null,
    ] {
        let mut value = payload();
        value["member"]["permissions"] = permission;
        assert!(
            parse(&interaction(value))
                .unwrap_err()
                .contains("Moderate Members")
        );
    }
}

#[test]
fn strike_context_requires_a_guild_and_target_membership() {
    let mut value = payload();
    value["guild_id"] = Value::Null;
    assert!(parse(&interaction(value)).unwrap_err().contains("server"));
    let mut value = payload();
    value["data"]["resolved"]["members"] = json!({});
    assert!(
        parse(&interaction(value))
            .unwrap_err()
            .contains("member of this server")
    );
}

#[test]
fn reason_validation_counts_characters_and_rejects_empty_input() {
    for reason in [" \t\n".to_owned(), "x".repeat(1001)] {
        let mut value = payload();
        value["data"]["options"][1]["value"] = json!(reason);
        assert!(
            parse(&interaction(value))
                .unwrap_err()
                .contains("1 and 1000")
        );
    }
    let mut value = payload();
    value["data"]["options"][1]["value"] = json!("é".repeat(1000));
    assert!(parse(&interaction(value)).is_ok());
    let mut value = payload();
    value["data"]["options"] = json!([]);
    assert!(parse(&interaction(value)).is_err());
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn strike_storage_smoke() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        8,
    )
    .await?;
    sqlx::migrate!().run(&pool).await?;
    let base = 9_000_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    let make_strike = |guild_id, interaction_id, reason: &str| NewStrike {
        guild_id,
        source: StrikeSource::Interaction(interaction_id),
        channel_id: base + 10,
        user_id: base + 100,
        moderator_id: base + 200,
        reason: reason.to_owned(),
    };
    let strike = std::sync::Arc::new(make_strike(
        base,
        base + 1,
        "Repeated spam; user's quoted text: 'hello'.",
    ));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let pool = pool.clone();
        let strike = std::sync::Arc::clone(&strike);
        tasks.spawn(async move { record(&pool, &strike).await.map(|saved| saved.strike.id) });
    }
    let mut ids = Vec::new();
    while let Some(result) = tasks.join_next().await {
        ids.push(result??);
    }
    assert!(ids.iter().all(|id| *id == ids[0]));

    // An interaction retry keeps the original audit fields intact.
    let before: time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM strikes WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await?;
    let mut retry = make_strike(base, base + 1, "changed reason");
    retry.moderator_id += 1;
    let duplicate = record(&pool, &retry).await?;
    assert!(!duplicate.created);
    assert_eq!(duplicate.strike.id, ids[0]);
    let saved: (i64, i64, i64, String, time::OffsetDateTime) = sqlx::query_as(
        "SELECT user_id, moderator_id, channel_id, reason, created_at FROM strikes WHERE id = $1",
    )
    .bind(ids[0])
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        saved,
        (
            strike.user_id,
            strike.moderator_id,
            strike.channel_id,
            strike.reason.clone(),
            before
        )
    );

    let second = record(&pool, &make_strike(base, base + 2, "A separate incident"))
        .await?
        .strike
        .id;
    assert_ne!(second, ids[0]);
    record(
        &pool,
        &make_strike(base + 1000, base + 3, "A different server"),
    )
    .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM strikes WHERE guild_id = $1 AND user_id = $2")
            .bind(base)
            .bind(strike.user_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(count, 2);
    assert!(
        record(&pool, &make_strike(base, base + 4, ""))
            .await
            .is_err()
    );
    assert!(
        record(&pool, &make_strike(base, base + 5, &"x".repeat(1001)))
            .await
            .is_err()
    );

    // The full handler also enforces permissions before it can write.
    let mut unauthorized = payload();
    unauthorized["member"]["permissions"] = json!("0");
    let moderation = crate::moderation::Moderation::new(
        pool.clone(),
        std::sync::Arc::new(twilight_http::Client::new("test-token".into())),
        jeeves::typesafe::Client::new("test-key")?,
        Id::new(600),
    );
    assert!(
        handle(&moderation, &interaction(unauthorized))
            .await
            .contains("Moderate Members")
    );
    sqlx::query("DELETE FROM strikes WHERE guild_id = ANY($1)")
        .bind(vec![base, base + 1000])
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}
