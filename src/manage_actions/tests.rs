use super::*;
use serde_json::json;
use twilight_model::id::Id;

fn interaction() -> Interaction {
    let mut value = crate::strikes::tests::payload();
    value["member"]["permissions"] = json!(Permissions::ADMINISTRATOR.bits().to_string());
    value["data"]["name"] = json!("manageactions");
    value["data"]["options"] = json!([]);
    serde_json::from_value(value).unwrap()
}

fn request() -> Request {
    Request {
        snapshot: Some(Snapshot {
            messages: 42,
            strikes: 43,
        }),
        ..Request::parse(&interaction(), None).unwrap()
    }
}

#[test]
fn permissions_and_button_ownership_are_checked_for_every_operation() {
    let request = request();
    for operation in [
        Operation::Page,
        Operation::Previous,
        Operation::Next,
        Operation::Remove {
            message: true,
            id: 42,
        },
        Operation::Remove {
            message: false,
            id: 42,
        },
        Operation::ConfirmClear,
        Operation::Clear,
    ] {
        let button = request.button_id(operation, 5);
        let parsed = Request::parse(&interaction(), Some(&button)).unwrap();
        assert_eq!(parsed.operation, operation);
        assert_eq!(parsed.snapshot, request.snapshot);
        assert_eq!(parsed.page, 5);
        for permissions in [
            None,
            Some(Permissions::empty()),
            Some(Permissions::MODERATE_MEMBERS),
        ] {
            let mut interaction = interaction();
            interaction.member.as_mut().unwrap().permissions = permissions;
            assert!(
                Request::parse(&interaction, None)
                    .unwrap_err()
                    .contains("Administrator")
            );
            assert!(
                Request::parse(&interaction, Some(&button))
                    .unwrap_err()
                    .contains("Administrator")
            );
        }
        for (guild, admin) in [(101, 400), (100, 401)] {
            let mut interaction = interaction();
            interaction.guild_id = Some(Id::new(guild));
            interaction
                .member
                .as_mut()
                .unwrap()
                .user
                .as_mut()
                .unwrap()
                .id = Id::new(admin);
            assert!(
                Request::parse(&interaction, Some(&button))
                    .unwrap_err()
                    .contains("another admin or server")
            );
        }
    }
    let mut dm = interaction();
    dm.guild_id = None;
    assert!(Request::parse(&dm, None).is_err());
}

#[test]
fn buttons_keep_table_identity_and_validate_cutoffs_and_size() {
    let request = request();
    assert_ne!(
        request.button_id(
            Operation::Remove {
                message: true,
                id: 42
            },
            0
        ),
        request.button_id(
            Operation::Remove {
                message: false,
                id: 42
            },
            0
        )
    );
    for id in [
        "actionadmin:".into(),
        "actionadmin:clear:%%%".into(),
        request.button_id(
            Operation::Remove {
                message: true,
                id: 43,
            },
            0,
        ),
        request.button_id(
            Operation::Remove {
                message: false,
                id: 44,
            },
            0,
        ),
        request.button_id(
            Operation::Remove {
                message: true,
                id: 0,
            },
            0,
        ),
        request.button_id(Operation::Next, -1),
        format!("{}:extra", request.button_id(Operation::Clear, 0)),
        request
            .button_id(Operation::Clear, 0)
            .replace(":clear:", ":unknown:"),
    ] {
        assert!(Request::parse(&interaction(), Some(&id)).is_err(), "{id}");
    }
    for snapshot in [
        Snapshot {
            messages: -1,
            strikes: 1,
        },
        Snapshot {
            messages: 0,
            strikes: 0,
        },
    ] {
        let invalid = Request {
            snapshot: Some(snapshot),
            ..request
        };
        assert!(
            Request::parse(&interaction(), Some(&invalid.button_id(Operation::Page, 0))).is_err()
        );
    }
    let only_strikes = Request {
        snapshot: Some(Snapshot {
            messages: 0,
            strikes: 43,
        }),
        ..request
    };
    assert!(
        Request::parse(
            &interaction(),
            Some(&only_strikes.button_id(
                Operation::Remove {
                    message: false,
                    id: 43
                },
                0
            ))
        )
        .is_ok()
    );
    assert!(
        Request::parse(
            &interaction(),
            Some(&only_strikes.button_id(
                Operation::Remove {
                    message: true,
                    id: 1
                },
                0
            ))
        )
        .is_err()
    );
    let maximum = Request {
        guild_id: i64::MAX,
        admin_id: i64::MAX,
        snapshot: Some(Snapshot {
            messages: i32::MAX,
            strikes: i32::MAX,
        }),
        page: i64::MAX,
        ..request
    };
    assert!(
        maximum
            .button_id(
                Operation::Remove {
                    message: false,
                    id: i32::MAX
                },
                i64::MAX
            )
            .len()
            <= 100
    );
}

#[test]
fn pages_show_rule_types_and_scopes_without_ids_and_fit_long_unicode() {
    for question in [
        "A short rule".to_owned(),
        "x".repeat(1000),
        "🦀".repeat(1000),
        "🦀".repeat(2000),
    ] {
        let size = page_size(question.chars().take(1000).collect::<String>().len() as i32);
        let actions = (0..size)
            .map(|i| Action {
                id: 40 + i as i32,
                message: i % 2 == 0,
                code: i % 3 == 0,
                question: question.clone(),
                only_channels: Some(vec![i64::MAX; 50]),
            })
            .collect();
        let page = Page {
            request: Request {
                snapshot: Some(Snapshot {
                    messages: 44,
                    strikes: 44,
                }),
                ..request()
            },
            total: 7,
            size,
            actions,
        };
        let (_, embeds, components) = render(page, "");
        assert_eq!(embeds[0].title.as_deref(), Some("Server actions"));
        assert_eq!(embeds[0].fields.len(), size as usize * 2);
        for fields in embeds[0].fields.as_chunks::<2>().0 {
            assert!(!fields[0].name.contains('#'));
            assert!(fields[1].value.contains("and 40 more"));
            assert!(fields[0].value.chars().count() <= 1000);
        }
        let _ = EmbedBuilder::from(embeds[0].clone()).validate().unwrap();
        let Component::ActionRow(navigation) = &components[0] else {
            panic!()
        };
        let Component::Button(previous) = &navigation.components[0] else {
            panic!()
        };
        let Component::Button(next) = &navigation.components[1] else {
            panic!()
        };
        assert!(previous.disabled && !next.disabled);
        let Component::ActionRow(removals) = &components[1] else {
            panic!()
        };
        assert_eq!(removals.components.len(), size as usize);
        for (i, component) in removals.components.iter().enumerate() {
            let Component::Button(button) = component else {
                panic!()
            };
            assert_eq!(
                button.label.as_deref(),
                Some(format!("Remove {}", i + 1).as_str())
            );
            let parsed = Request::parse(&interaction(), button.custom_id.as_deref()).unwrap();
            assert_eq!(
                parsed.operation,
                Operation::Remove {
                    message: i % 2 == 0,
                    id: 40 + i as i32
                }
            );
        }
    }
    for (message, code, expected) in [
        (true, false, "Message · Interpretation"),
        (true, true, "Message · Code"),
        (false, false, "Strike · Interpretation"),
        (false, true, "Strike · Code"),
    ] {
        let mut action = Action {
            id: 1,
            message,
            code,
            question: "rule".into(),
            only_channels: None,
        };
        assert_eq!(action.kind(), expected);
        assert_eq!(action.channels(), "All channels");
        action.only_channels = Some(vec![]);
        assert_eq!(action.channels(), "No channels (inactive)");
        action.only_channels = Some(vec![200, 201]);
        assert_eq!(action.channels(), "<#200>, <#201>");
    }
}

async fn insert(pool: &PgPool, guild: i64, message: bool, id: i32) -> Result<()> {
    sqlx::query(if message {
        "INSERT INTO message_actions (id, guild_id, question, code, only_channels) VALUES ($1, $2, $3, $4, $5)"
    } else {
        "INSERT INTO strike_actions (id, guild_id, question, code, only_channels) VALUES ($1, $2, $3, $4, $5)"
    }).bind(id).bind(guild).bind(format!("Test {} rule", if message {"message"} else {"strike"}))
        .bind((id % 2 == 0).then_some("items => null"))
        .bind(message.then_some(vec![200_i64])).execute(pool).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn action_management_smoke() -> Result<()> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        4,
    )
    .await?;
    let guild = 9_800_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    let base = i32::try_from(std::process::id())? * 100;
    let mut request = Request {
        guild_id: guild,
        snapshot: None,
        ..request()
    };
    let empty = load_page(&pool, request).await?;
    let (content, embeds, components) = render(empty, "");
    assert!(content.contains("No actions"));
    assert!(embeds.is_empty() && components.is_empty());
    for message in [true, false] {
        insert(&pool, guild + 1, message, base).await?;
        for n in 1..=3 {
            insert(&pool, guild, message, base + n).await?;
        }
    }
    let first = load_page(&pool, request).await?;
    request = first.request;
    assert_eq!(first.total, 6);
    assert_eq!(
        first
            .actions
            .iter()
            .map(|a| (a.message, a.id))
            .collect::<Vec<_>>(),
        [
            (true, base + 3),
            (true, base + 2),
            (true, base + 1),
            (false, base + 3),
            (false, base + 2)
        ]
    );
    let last = load_page(
        &pool,
        Request {
            page: i64::MAX,
            ..request
        },
    )
    .await?;
    assert_eq!(last.request.page, 1);
    assert_eq!(last.actions.len(), 1);
    assert_eq!(
        (last.actions[0].message, last.actions[0].id),
        (false, base + 1)
    );
    let (_, _, controls) = render(last, "");
    let Component::ActionRow(navigation) = &controls[0] else {
        panic!()
    };
    let Component::Button(next) = &navigation.components[1] else {
        panic!()
    };
    assert!(next.disabled);

    let confirm = Request {
        operation: Operation::ConfirmClear,
        ..request
    };
    assert!(remove(&pool, confirm).await.is_err());
    let confirmation = render(load_page(&pool, confirm).await?, "");
    assert!(confirmation.0.contains("Clear all 6 listed actions"));
    assert!(confirmation.1.is_empty());
    assert!(remove(&pool, request).await.is_err());
    for message in [true, false] {
        assert_eq!(
            remove(
                &pool,
                Request {
                    operation: Operation::Remove { message, id: base },
                    ..request
                }
            )
            .await?,
            0
        );
    }
    let delete = Request {
        operation: Operation::Remove {
            message: true,
            id: base + 3,
        },
        ..request
    };
    let (a, b) = tokio::join!(remove(&pool, delete), remove(&pool, delete));
    assert_eq!(a? + b?, 1);
    let after = load_page(
        &pool,
        Request {
            page: i64::MAX,
            ..request
        },
    )
    .await?;
    assert_eq!(after.total, 5);
    assert_eq!(after.request.page, 0);
    assert!(after.actions.iter().any(|a| !a.message && a.id == base + 3));
    for message in [true, false] {
        insert(&pool, guild, message, base + 4).await?;
    }
    assert_eq!(load_page(&pool, request).await?.total, 5);
    assert_eq!(
        load_page(
            &pool,
            Request {
                snapshot: None,
                ..request
            }
        )
        .await?
        .total,
        7
    );
    for message in [true, false] {
        assert_eq!(
            remove(
                &pool,
                Request {
                    operation: Operation::Remove {
                        message,
                        id: base + 4
                    },
                    ..request
                }
            )
            .await?,
            0
        );
    }
    let clear = Request {
        operation: Operation::Clear,
        ..request
    };
    assert_eq!(remove(&pool, clear).await?, 5);
    assert_eq!(remove(&pool, clear).await?, 0);
    let (content, embeds, controls) = render(load_page(&pool, request).await?, "Removed actions.");
    assert!(content.contains("No listed actions remain"));
    assert!(embeds.is_empty() && controls.is_empty());
    let refreshed = load_page(
        &pool,
        Request {
            snapshot: None,
            ..request
        },
    )
    .await?;
    assert_eq!(
        refreshed
            .actions
            .iter()
            .map(|a| (a.message, a.id))
            .collect::<Vec<_>>(),
        [(true, base + 4), (false, base + 4)]
    );
    for query in [
        "SELECT COUNT(*) FROM message_actions WHERE guild_id = $1",
        "SELECT COUNT(*) FROM strike_actions WHERE guild_id = $1",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .bind(guild + 1)
                .fetch_one(&pool)
                .await?,
            1
        );
    }
    for query in [
        "DELETE FROM message_actions WHERE guild_id = ANY($1)",
        "DELETE FROM strike_actions WHERE guild_id = ANY($1)",
    ] {
        sqlx::query(query)
            .bind(vec![guild, guild + 1])
            .execute(&pool)
            .await?;
    }
    pool.close().await;
    Ok(())
}
