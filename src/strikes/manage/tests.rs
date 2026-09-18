use super::*;
use serde_json::json;

fn interaction() -> Interaction {
    let mut value = crate::strikes::tests::payload();
    value["member"]["permissions"] = json!(Permissions::ADMINISTRATOR.bits().to_string());
    value["data"]["name"] = json!("managestrikes");
    value["data"]["options"] = json!([{"name": "user", "type": 6, "value": "300"}]);
    // A banned/former member can still have strikes in this server.
    value["data"]["resolved"]["members"] = json!({});
    serde_json::from_value(value).unwrap()
}

fn request() -> Request {
    let mut request = Request::parse(&interaction(), None).unwrap();
    request.history.snapshot = Some(42);
    request
}

#[test]
fn admins_can_manage_other_users_and_permission_is_rechecked_on_every_button() {
    let mut interaction = interaction();
    let request = request();
    assert_eq!(
        (
            request.history.guild_id,
            request.history.user_id,
            request.admin_id
        ),
        (100, 300, 400)
    );
    let buttons: Vec<_> = [
        Operation::Page,
        Operation::Remove(42),
        Operation::ConfirmClear,
        Operation::Clear,
    ]
    .into_iter()
    .map(|op| request.button_id(op, 0))
    .collect();
    for permissions in [
        Some(Permissions::empty()),
        Some(Permissions::MODERATE_MEMBERS),
        None,
    ] {
        interaction.member.as_mut().unwrap().permissions = permissions;
        assert!(
            Request::parse(&interaction, None)
                .unwrap_err()
                .contains("Administrator")
        );
        for id in &buttons {
            assert!(
                Request::parse(&interaction, Some(id))
                    .unwrap_err()
                    .contains("Administrator")
            );
        }
    }
    interaction.member.as_mut().unwrap().permissions = Some(Permissions::ADMINISTRATOR);
    interaction.guild_id = None;
    assert!(
        Request::parse(&interaction, None)
            .unwrap_err()
            .contains("server")
    );
}

#[test]
fn buttons_are_bound_to_the_admin_and_guild_and_preserve_exact_strike_id() {
    let request = request();
    for operation in [
        Operation::Previous,
        Operation::Next,
        Operation::Remove(42),
        Operation::ConfirmClear,
        Operation::Clear,
    ] {
        let id = request.button_id(operation, 7);
        let parsed = Request::parse(&interaction(), Some(&id)).unwrap();
        assert_eq!(parsed.operation, operation);
        assert_eq!(parsed.history.user_id, 300);
        assert_eq!(parsed.history.snapshot, Some(42));
        assert_eq!(parsed.history.page, 7);
        for (guild_id, admin_id) in [(101, 400), (100, 401)] {
            let mut other = interaction();
            other.guild_id = Some(twilight_model::id::Id::new(guild_id));
            other.member.as_mut().unwrap().user.as_mut().unwrap().id =
                twilight_model::id::Id::new(admin_id);
            assert!(
                Request::parse(&other, Some(&id))
                    .unwrap_err()
                    .contains("another admin or server")
            );
        }
    }
    let maximum = Request {
        history: HistoryRequest {
            guild_id: i64::MAX,
            user_id: i64::MAX,
            snapshot: Some(i64::MAX),
            page: i64::MAX,
        },
        admin_id: i64::MAX,
        operation: Operation::Page,
    };
    assert!(
        maximum
            .button_id(Operation::Remove(i64::MAX), i64::MAX)
            .len()
            <= 100
    );
    assert!(maximum.button_id(Operation::ConfirmClear, i64::MAX).len() <= 100);
}

#[test]
fn malformed_buttons_and_missing_targets_are_rejected() {
    let request = request();
    let mut malformed = vec![
        "strikeadmin:".into(),
        "strikeadmin:page:%%%".into(),
        "strikeadmin:clear:AA".into(),
        request.button_id(Operation::Page, -1),
        request.button_id(Operation::Remove(43), 0),
        request.button_id(Operation::Remove(0), 0),
        format!("{}:extra", request.button_id(Operation::Page, 0)),
        request
            .button_id(Operation::Page, 0)
            .replace(":page:", ":unknown:"),
    ];
    for (user, cutoff) in [(0, Some(42)), (300, Some(0)), (300, None), (-1, Some(42))] {
        let mut bad = request;
        bad.history.user_id = user;
        bad.history.snapshot = cutoff;
        malformed.push(bad.button_id(Operation::Page, 0));
    }
    for id in malformed {
        assert!(Request::parse(&interaction(), Some(&id)).is_err(), "{id}");
    }
    let mut missing = interaction();
    let Some(InteractionData::ApplicationCommand(command)) = missing.data.as_mut() else {
        panic!()
    };
    command.options.clear();
    assert!(
        Request::parse(&missing, None)
            .unwrap_err()
            .contains("Choose a user")
    );
    assert_eq!(options()[0].required, Some(true));
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn admin_strike_management_smoke() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let base = 9_500_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    let make_strike = |guild_id, user_id, interaction_id| super::super::NewStrike {
        guild_id,
        user_id,
        channel_id: base + 200,
        moderator_id: base + 300,
        source: super::super::StrikeSource::Interaction(interaction_id),
        reason: "Test moderation reason".into(),
    };
    let mut request = Request {
        history: HistoryRequest {
            guild_id: base,
            user_id: base + 100,
            snapshot: None,
            page: 0,
        },
        admin_id: base + 300,
        operation: Operation::Page,
    };
    let empty = history::load_page(&pool, request.history).await?;
    let (content, embeds, components) = render(empty, request, "");
    assert!(content.contains("no strikes"));
    assert!(embeds.is_empty() && components.is_empty());
    let mut ids = vec![];
    for n in 0..6 {
        ids.push(
            super::super::record(&pool, &make_strike(base, base + 100, base + n))
                .await?
                .strike
                .id,
        );
    }
    let other_user = super::super::record(&pool, &make_strike(base, base + 101, base + 10))
        .await?
        .strike
        .id;
    let other_guild = super::super::record(&pool, &make_strike(base + 1, base + 100, base + 11))
        .await?
        .strike
        .id;
    let page = history::load_page(&pool, request.history).await?;
    request.history = page.request;
    assert_eq!(page.total, 6);
    let (content, embeds, components) = render(page, request, "");
    assert!(content.contains(&format!("<@{}>", base + 100)));
    assert_eq!(embeds[0].title.as_deref(), Some("Manage strikes"));
    assert_eq!(embeds[0].fields.len(), 10);
    assert_eq!(
        embeds[0].footer.as_ref().unwrap().text,
        "Page 1 of 2 · 6 strikes · Newest first"
    );
    let Component::ActionRow(row) = &components[0] else {
        panic!()
    };
    let mut custom_ids = std::collections::HashSet::new();
    for component in &row.components {
        let Component::Button(button) = component else {
            panic!()
        };
        assert!(custom_ids.insert(button.custom_id.as_ref().unwrap()));
    }
    assert_eq!(row.components.len(), 3);
    let Component::ActionRow(removals) = &components[1] else {
        panic!()
    };
    assert_eq!(removals.components.len(), 5);
    for (index, component) in removals.components.iter().enumerate() {
        let Component::Button(button) = component else {
            panic!()
        };
        assert_eq!(
            button.label.as_deref(),
            Some(format!("Remove {}", index + 1).as_str())
        );
        assert!(custom_ids.insert(button.custom_id.as_ref().unwrap()));
        let mut interaction = interaction();
        interaction.guild_id = Some(twilight_model::id::Id::new(base as u64));
        interaction
            .member
            .as_mut()
            .unwrap()
            .user
            .as_mut()
            .unwrap()
            .id = twilight_model::id::Id::new((base + 300) as u64);
        let parsed = Request::parse(&interaction, button.custom_id.as_deref()).unwrap();
        assert_eq!(parsed.operation, Operation::Remove(ids[5 - index]));
    }
    let second = history::load_page(
        &pool,
        HistoryRequest {
            page: 1,
            ..request.history
        },
    )
    .await?;
    assert_eq!(second.strikes.len(), 1);
    assert_eq!(second.strikes[0].id, ids[0]);

    // These operations must never cross the selected user's or server's boundary.
    for id in [other_user, other_guild] {
        let mut wrong_target = request;
        wrong_target.history.snapshot = Some(id);
        wrong_target.operation = Operation::Remove(id);
        assert_eq!(remove(&pool, wrong_target).await?, 0);
    }
    // Confirming clear is only a view; cancellation is also read-only.
    request.operation = Operation::ConfirmClear;
    let confirmation = render(
        history::load_page(&pool, request.history).await?,
        request,
        "",
    );
    assert!(confirmation.0.contains("Clear all 6 listed strikes"));
    assert!(confirmation.1.is_empty());
    assert!(remove(&pool, request).await.is_err());
    request.operation = Operation::Page;
    assert!(remove(&pool, request).await.is_err());
    assert_eq!(history::load_page(&pool, request.history).await?.total, 6);

    // A stale remove button targets its exact ID, never another row at the same offset.
    request.operation = Operation::Remove(ids[5]);
    let (a, b) = tokio::join!(remove(&pool, request), remove(&pool, request));
    assert_eq!(a? + b?, 1);
    request.history.page = i64::MAX;
    let last = history::load_page(&pool, request.history).await?;
    assert_eq!(last.total, 5);
    assert_eq!(last.request.page, 0);
    assert_eq!(last.strikes.len(), 5);
    assert_eq!(last.strikes.last().unwrap().id, ids[0]);

    // New strikes added after opening the view survive even a repeated clear.
    let fresh = super::super::record(&pool, &make_strike(base, base + 100, base + 12))
        .await?
        .strike
        .id;
    request.operation = Operation::Clear;
    assert_eq!(remove(&pool, request).await?, 5);
    assert_eq!(remove(&pool, request).await?, 0);
    let remaining: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM strikes WHERE guild_id = ANY($1) ORDER BY id")
            .bind(vec![base, base + 1])
            .fetch_all(&pool)
            .await?;
    assert_eq!(remaining, vec![other_user, other_guild, fresh]);
    assert_eq!(history::load_page(&pool, request.history).await?.total, 0);
    request.history.snapshot = None;
    let refreshed = history::load_page(&pool, request.history).await?;
    assert_eq!(refreshed.total, 1);
    let (_, _, components) = render(refreshed, request, "");
    let Component::ActionRow(row) = &components[0] else {
        panic!()
    };
    let buttons: Vec<_> = row
        .components
        .iter()
        .map(|c| match c {
            Component::Button(b) => b,
            _ => panic!(),
        })
        .collect();
    assert!(buttons[0].disabled && buttons[1].disabled);
    assert_ne!(buttons[0].custom_id, buttons[1].custom_id);
    sqlx::query("DELETE FROM strikes WHERE guild_id = ANY($1)")
        .bind(vec![base, base + 1])
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}
