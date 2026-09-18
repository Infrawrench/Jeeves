use super::*;
use serde_json::json;

fn interaction() -> Interaction {
    serde_json::from_value(json!({
        "id": "500", "application_id": "600", "type": 2, "token": "test-token",
        "guild_id": "100", "entitlements": [],
        "authorizing_integration_owners": {"0": "100"},
        "member": {
            "user": {"id": "300", "username": "recipient", "discriminator": "0"},
            "roles": [], "deaf": false, "mute": false, "flags": 0, "permissions": "0"
        },
        "data": {"id": "700", "name": "strikes", "type": 1}
    }))
    .unwrap()
}

#[test]
fn any_member_can_view_only_their_own_server_history() {
    let mut interaction = interaction();
    let request = HistoryRequest::parse(&interaction, None).unwrap();
    assert_eq!(
        (request.guild_id, request.user_id, request.page),
        (100, 300, 0)
    );
    assert!(request.snapshot.is_none());
    assert!(HistoryRequest::parse(&interaction, Some("strikes:next:100:300:42:1")).is_ok());
    for custom_id in ["strikes:next:100:301:42:1", "strikes:next:101:300:42:1"] {
        assert!(
            HistoryRequest::parse(&interaction, Some(custom_id))
                .unwrap_err()
                .contains("someone else's")
        );
    }
    interaction.guild_id = None;
    assert!(
        HistoryRequest::parse(&interaction, None)
            .unwrap_err()
            .contains("server")
    );
}

#[test]
fn malformed_buttons_are_rejected_and_large_ids_fit_discord() {
    let interaction = interaction();
    for custom_id in [
        "strikes:",
        "strikes:next:100:300:42",
        "strikes:next:100:300:42:1:extra",
        "strikes:next:100:300:0:1",
        "strikes:next:100:300:-1:1",
        "strikes:next:100:300:42:-1",
        "strikes:next:100:300:42:9223372036854775808",
        "strikes:unknown:100:300:42:1",
        "strikes:next:100:300:42:nope",
    ] {
        assert!(
            HistoryRequest::parse(&interaction, Some(custom_id)).is_err(),
            "{custom_id}"
        );
    }
    let request = HistoryRequest {
        guild_id: i64::MAX,
        user_id: i64::MAX,
        snapshot: Some(i64::MAX),
        page: i64::MAX - 1,
    };
    assert!(request.button_id("prev", request.page).len() <= 100);
}

fn page(index: i64, total: i64, reason: &str) -> Page {
    let size = page_size(reason.len() as i32);
    Page {
        request: HistoryRequest {
            guild_id: 100,
            user_id: 300,
            snapshot: Some(42),
            page: index,
        },
        total,
        size,
        strikes: (0..(total - index * size).clamp(0, size))
            .map(|_| Strike {
                id: 42,
                moderator_id: 400,
                reason: reason.to_owned(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
            })
            .collect(),
    }
}

#[test]
fn pages_show_complete_reasons_and_disable_boundary_buttons() {
    for reason in ["x".repeat(1000), "🦀".repeat(1000)] {
        let size = page_size(reason.len() as i32);
        for (index, total, disabled) in [
            (0, size * 2 + 1, [true, false]),
            (1, size * 2 + 1, [false, false]),
            (2, size * 2 + 1, [false, true]),
            (0, size, [true, true]),
            (1, size + 1, [false, true]),
        ] {
            let (content, embeds, components) = page(index, total, &reason).render();
            assert!(content.is_empty());
            assert_eq!(embeds[0].title.as_deref(), Some("Your strikes"));
            let count = (total - index * size).min(size) as usize;
            assert_eq!(embeds[0].fields.len(), count * 2);
            for (i, fields) in embeds[0].fields.as_chunks::<2>().0.iter().enumerate() {
                assert_eq!(fields[0].name, format!("Strike {}", i + 1));
                assert_eq!(fields[0].value, reason);
                assert!(fields[1].value.contains("<@400>"));
                assert!(fields[1].value.contains("<t:0:f>"));
            }
            let pages = (total - 1) / size + 1;
            assert!(
                embeds[0]
                    .footer
                    .as_ref()
                    .unwrap()
                    .text
                    .starts_with(&format!("Page {} of {pages} · {total} strike", index + 1))
            );
            let _ = EmbedBuilder::from(embeds[0].clone()).validate().unwrap();
            let Component::ActionRow(row) = &components[0] else {
                panic!("expected action row")
            };
            let mut ids = Vec::new();
            for (component, disabled) in row.components.iter().zip(disabled) {
                let Component::Button(button) = component else {
                    panic!("expected button")
                };
                assert_eq!(button.disabled, disabled);
                ids.push(button.custom_id.as_ref().unwrap());
                let request =
                    HistoryRequest::parse(&interaction(), button.custom_id.as_deref()).unwrap();
                assert_eq!(request.snapshot, Some(42));
                assert!((0..pages).contains(&request.page));
            }
            assert_ne!(ids[0], ids[1]);
        }
    }
    let (content, embeds, components) = page(0, 0, "").render();
    assert!(content.contains("no strikes"));
    assert!(embeds.is_empty() && components.is_empty());
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn strike_history_smoke() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let base = 9_100_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    let request = HistoryRequest {
        guild_id: base,
        user_id: base + 100,
        snapshot: None,
        page: 0,
    };
    let make_strike = |guild_id, user_id, interaction_id| super::super::NewStrike {
        guild_id,
        user_id,
        source: super::super::StrikeSource::Interaction(interaction_id),
        channel_id: base + 200,
        moderator_id: base + 300,
        reason: "Repeated spam".into(),
    };
    assert_eq!(load_page(&pool, request).await?.total, 0);
    let mut ids = Vec::new();
    for index in 0..7 {
        ids.push(
            super::super::record(&pool, &make_strike(base, base + 100, base + index))
                .await?
                .strike
                .id,
        );
    }
    // These must not appear in the requester's history or count.
    super::super::record(&pool, &make_strike(base + 1, base + 100, base + 10)).await?;
    super::super::record(&pool, &make_strike(base, base + 101, base + 11)).await?;
    sqlx::query("UPDATE strikes SET created_at = '2025-01-01T00:00:00Z' WHERE guild_id = $1")
        .bind(base)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE strikes SET created_at = '2025-01-02T00:00:00Z' WHERE id = $1")
        .bind(ids[0])
        .execute(&pool)
        .await?;

    let first = load_page(&pool, request).await?;
    assert_eq!(first.total, 7);
    assert_eq!(first.strikes.len(), 5);
    assert_eq!(first.strikes[0].id, ids[0]);
    let new_id = super::super::record(&pool, &make_strike(base, base + 100, base + 12))
        .await?
        .strike
        .id;
    let expected = [ids[0], ids[6], ids[5], ids[4], ids[3], ids[2], ids[1]];
    for (index, expected_ids) in expected.chunks(PAGE_SIZE as usize).enumerate() {
        let page = load_page(
            &pool,
            HistoryRequest {
                page: index as i64,
                ..first.request
            },
        )
        .await?;
        assert_eq!(page.total, 7);
        assert_eq!(
            page.strikes
                .iter()
                .map(|strike| strike.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
    }
    // A new command includes the new strike, while existing navigation keeps its cutoff.
    let refreshed = load_page(&pool, request).await?;
    assert_eq!(refreshed.total, 8);
    assert_eq!(refreshed.strikes[0].id, new_id);
    let last = load_page(
        &pool,
        HistoryRequest {
            page: i64::MAX,
            ..first.request
        },
    )
    .await?;
    assert_eq!(last.request.page, 1);
    assert_eq!(last.strikes.last().unwrap().id, ids[1]);
    let backwards = load_page(
        &pool,
        HistoryRequest {
            page: 0,
            ..first.request
        },
    )
    .await?;
    assert_eq!(backwards.strikes[0].id, ids[0]);

    sqlx::query("DELETE FROM strikes WHERE guild_id = $1 AND user_id = $2")
        .bind(base)
        .bind(request.user_id)
        .execute(&pool)
        .await?;
    let empty = load_page(&pool, first.request).await?;
    assert_eq!(empty.total, 0);
    assert!(empty.strikes.is_empty());
    sqlx::query("DELETE FROM strikes WHERE guild_id = ANY($1)")
        .bind(vec![base, base + 1])
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}
