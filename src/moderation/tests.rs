use super::*;
use jeeves::message_actions::{ActionError, MessageAction};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

fn outcomes(values: Vec<MessageActionOutcome>) -> Vec<ActionResult<MessageActionOutcome>> {
    values
        .into_iter()
        .enumerate()
        .map(|(id, outcome)| ActionResult {
            action_id: id as i32,
            result: Ok(outcome),
        })
        .collect()
}

#[test]
fn reconciliation_keeps_every_strike_and_only_the_strongest_removal() {
    for reverse in [false, true] {
        let mut values = vec![
            MessageActionOutcome::Kick("kick-1".into()),
            MessageActionOutcome::Strike("strike-1".into()),
            MessageActionOutcome::Ban("ban-1".into()),
            MessageActionOutcome::Ignore,
            MessageActionOutcome::Ban("ban-2".into()),
            MessageActionOutcome::Strike("strike-2".into()),
            MessageActionOutcome::Kick("kick-2".into()),
        ];
        if reverse {
            values.reverse();
        }
        let mut plan = Plan::default();
        plan.include(outcomes(values));
        assert!(matches!(plan.removal, Some(Removal::Ban(_))));
        assert_eq!(plan.strikes.len(), 2);
        assert_eq!(plan.failures, 0);
    }
    let mut plan = Plan::default();
    plan.include(outcomes(vec![
        MessageActionOutcome::Kick("first".into()),
        MessageActionOutcome::Kick("second".into()),
        MessageActionOutcome::Strike("keep this".into()),
    ]));
    plan.include(vec![ActionResult {
        action_id: 99,
        result: Err(ActionError::Handler(anyhow::anyhow!("failed"))),
    }]);
    assert_eq!(plan.removal, Some(Removal::Kick("first".into())));
    assert_eq!(plan.strikes, [(2, "keep this".into())]);
    assert_eq!(plan.failures, 1);
}

fn message_context(base: i64, codes: &[(i32, &str)]) -> MessageContext {
    MessageContext {
        message: serde_json::from_value(json!({
            "id": (base + 4).to_string(), "guild_id": base.to_string(),
            "channel_id": (base + 1).to_string(),
            "author": {"id": (base + 2).to_string(), "username":"test", "discriminator":"0"},
            "content":"incoming", "timestamp":"2026-01-01T00:00:00+00:00",
            "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
            "attachments":[], "embeds":[], "pinned":false, "type":0,
        }))
        .unwrap(),
        images: vec![],
        history: vec![],
        actions: codes
            .iter()
            .map(|(id, code)| MessageAction {
                id: *id,
                guild_id: base,
                only_channels: None,
                role_id: None,
                question: format!("message rule {id}"),
                code: Some(format!("messages => {code}")),
            })
            .collect(),
    }
}

fn jev() -> typesafe::Client {
    typesafe::Client::builder("test-key")
        .base_url("http://127.0.0.1:1/v1")
        .build()
        .unwrap()
}

#[tokio::test]
async fn role_changes_coalesce_and_discord_failures_do_not_discard_other_actions() -> Result<()> {
    let pool = PgPool::connect_lazy("postgres://localhost:1/unused")?;
    for failed in [false, true] {
        let (http, requests) = discord_server(3, move |headers| {
            if failed && headers.starts_with("put /api/v10/guilds/100/members/102/roles/700 ") {
                (403, json!({"code":50013,"message":"Missing Permissions"}))
            } else {
                (204, json!({}))
            }
        })?;
        let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
        let mut input = message_context(
            100,
            &[
                (1, "'GIVE_ROLE'"),
                (2, "'GIVE_ROLE'"),
                (3, "'GIVE_ROLE'"),
                (4, "'REVOKE_ROLE'"),
                (5, "'GIVE_ROLE'"),
                (6, "'KICK'"),
            ],
        );
        for (action, role_id) in input.actions.iter_mut().zip([700, 700, 701, 701, 701]) {
            action.role_id = Some(role_id);
        }
        assert_eq!(moderation.process_message(input).await.is_err(), failed);
        let requests = requests.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]
                .headers
                .starts_with("put /api/v10/guilds/100/members/102/roles/700 ")
        );
        assert!(
            requests[1]
                .headers
                .starts_with("delete /api/v10/guilds/100/members/102/roles/701 ")
        );
        assert!(
            requests[2]
                .headers
                .starts_with("delete /api/v10/guilds/100/members/102 ")
        );
        for request in requests {
            assert!(request.headers.contains("x-audit-log-reason:"));
            assert!(request.body.is_null());
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn manual_and_automatic_strikes_apply_configured_role_changes() -> Result<()> {
    let pool = test_pool(4).await?;
    let base = 9_650_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    for (index, manual) in [true, false].into_iter().enumerate() {
        let guild = base + index as i64 * 100;
        for (role, code) in [
            (700, "strikes => 'GIVE_ROLE'"),
            (701, "strikes => 'REVOKE_ROLE'"),
        ] {
            let id = rule(&pool, guild, None, code).await?;
            sqlx::query("UPDATE strike_actions SET role_id = $1 WHERE id = $2")
                .bind(role as i64)
                .bind(id)
                .execute(&pool)
                .await?;
        }
        let (http, requests) =
            discord_with_hierarchy(if manual { 6 } else { 7 }, [204, 200, 204], guild, 600)?;
        let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
        if manual {
            let (_, failures) = moderation
                .record_manual(&NewStrike {
                    guild_id: guild,
                    channel_id: guild + 1,
                    user_id: guild + 2,
                    moderator_id: 600,
                    reason: "test role escalation".into(),
                    source: StrikeSource::Interaction(guild + 10),
                })
                .await?;
            assert_eq!(failures, 0);
        } else {
            moderation
                .process_message(message_context(guild, &[(1, "'STRIKE'")]))
                .await?;
        }
        let requests = requests.join().unwrap();
        assert!(requests.iter().any(|r| r.headers.starts_with(&format!(
            "put /api/v10/guilds/{guild}/members/{}/roles/700 ",
            guild + 2
        ))));
        assert!(requests.iter().any(|r| r.headers.starts_with(&format!(
            "delete /api/v10/guilds/{guild}/members/{}/roles/701 ",
            guild + 2
        ))));
        assert_feedback(
            &requests,
            guild + 1,
            guild + 2,
            (!manual).then_some(guild + 4),
            1,
        );
    }
    cleanup(&pool, &[base, base + 100]).await?;
    pool.close().await;
    Ok(())
}

async fn test_pool(size: u32) -> Result<PgPool> {
    crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        size,
    )
    .await
}

async fn rule(pool: &PgPool, guild: i64, channels: Option<Vec<i64>>, code: &str) -> Result<i32> {
    Ok(sqlx::query_scalar(
        "INSERT INTO strike_actions (guild_id, only_channels, question, code)
         VALUES ($1, $2, 'strike escalation', $3) RETURNING id",
    )
    .bind(guild)
    .bind(channels)
    .bind(code)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn moderation_pipeline_smoke() -> Result<()> {
    let pool = test_pool(8).await?;
    let base = 9_500_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    rule(
        &pool,
        base,
        None,
        "strikes => strikes.length >= 2 ? 'BAN' : 'KICK'",
    )
    .await?;
    rule(&pool, base, Some(vec![base + 1]), "strikes => 'KICK'").await?;
    let (http, requests) = discord_with_hierarchy(6, [204, 200, 204], base, base + 3)?;
    let moderation = Moderation::new(pool.clone(), http, jev(), Id::new((base + 3) as u64));
    moderation
        .process_message(message_context(
            base,
            &[
                (1, "'KICK'"),
                (2, "'STRIKE'"),
                (3, "'STRIKE'"),
                (4, "'KICK'"),
            ],
        ))
        .await?;
    let requests = requests.join().unwrap();
    assert_feedback(&requests, base + 1, base + 2, Some(base + 4), 2);
    let request = requests
        .iter()
        .find(|request| request.headers.starts_with("put "))
        .unwrap();
    assert!(
        request
            .headers
            .starts_with(&format!("put /api/v10/guilds/{base}/bans/{} ", base + 2))
    );
    assert_eq!(request.body["delete_message_seconds"], 0);
    assert!(request.headers.contains("x-audit-log-reason:"));
    let rows = sqlx::query_as::<_, (i64, Option<i64>, i64, i32)>(
        "SELECT moderator_id, interaction_id, source_message_id, source_action_id
         FROM strikes WHERE guild_id = $1 ORDER BY id",
    )
    .bind(base)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        rows,
        [(base + 3, None, base + 4, 2), (base + 3, None, base + 4, 3)]
    );

    // Replaying these strike outcomes neither inserts nor reevaluates their rules.
    let (http, retry_requests) = discord_with_hierarchy(3, [204, 200, 204], base, base + 3)?;
    let moderation = Moderation::new(pool.clone(), http, jev(), Id::new((base + 3) as u64));
    moderation
        .process_message(message_context(base, &[(2, "'STRIKE'"), (3, "'STRIKE'")]))
        .await?;
    assert!(
        retry_requests
            .join()
            .unwrap()
            .iter()
            .all(|request| request.headers.starts_with("get "))
    );
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = pool.clone();
        tasks.spawn(async move {
            strikes::record(
                &pool,
                &NewStrike {
                    guild_id: base,
                    channel_id: base + 1,
                    user_id: base + 2,
                    moderator_id: base + 3,
                    reason: "retry".into(),
                    source: StrikeSource::Message {
                        message_id: base + 4,
                        action_id: 2,
                    },
                },
            )
            .await
        });
    }
    while let Some(saved) = tasks.join_next().await {
        assert!(!saved??.created);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strikes WHERE guild_id = $1")
        .bind(base)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2);

    // /strike uses the same rules, collapses duplicate kicks, and reports partial failures.
    let manual_guild = base + 100;
    rule(
        &pool,
        manual_guild,
        None,
        "strikes => strikes.length === 1 ? 'KICK' : null",
    )
    .await?;
    rule(&pool, manual_guild, None, "strikes => 'KICK'").await?;
    rule(
        &pool,
        manual_guild,
        None,
        "strikes => { throw new Error('bad rule'); }",
    )
    .await?;
    let (http, requests) = discord_with_hierarchy(8, [204, 200, 204], manual_guild, 600)?;
    let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
    let mut interaction = crate::strikes::tests::payload();
    interaction["id"] = json!((base + 500).to_string());
    interaction["guild_id"] = json!(manual_guild.to_string());
    let interaction = serde_json::from_value(interaction)?;
    let response = strikes::handle(&moderation, &interaction).await;
    assert!(response.starts_with("Recorded a strike for"));
    assert!(response.contains("Some strike notifications or actions failed"));
    let duplicate = strikes::handle(&moderation, &interaction).await;
    assert!(duplicate.starts_with("Recorded a strike for"));
    let requests = requests.join().unwrap();
    assert_feedback(&requests, 200, 300, None, 1);
    assert!(
        requests
            .iter()
            .any(|request| request.headers.starts_with(&format!(
                "delete /api/v10/guilds/{manual_guild}/members/300 "
            )))
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strikes WHERE guild_id = $1")
        .bind(manual_guild)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);

    // Discord failure does not undo the strikes, or cause a fallback kick.
    let failed_guild = base + 200;
    let (http, requests) = discord_with_hierarchy(6, [403, 403, 403], failed_guild, 600)?;
    let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
    assert!(
        moderation
            .process_message(message_context(
                failed_guild,
                &[
                    (1, "'BAN'"),
                    (2, "'KICK'"),
                    (3, "'STRIKE'"),
                    (4, "'STRIKE'"),
                ]
            ))
            .await
            .is_err()
    );
    let requests = requests.join().unwrap();
    assert_feedback(
        &requests,
        failed_guild + 1,
        failed_guild + 2,
        Some(failed_guild + 4),
        2,
    );
    assert!(
        requests
            .iter()
            .any(|request| request.headers.starts_with(&format!(
                "put /api/v10/guilds/{failed_guild}/bans/{} ",
                failed_guild + 2
            )))
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strikes WHERE guild_id = $1")
        .bind(failed_guild)
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2);
    cleanup(&pool, &[base, manual_guild, failed_guild]).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL; see README.md"]
async fn strike_action_context_smoke() -> Result<()> {
    let pool = test_pool(1).await?;
    let base = 9_600_000_000_000_000_i64 + i64::from(std::process::id()) * 100_000;
    let make = |guild, user, id, channel| NewStrike {
        guild_id: guild,
        channel_id: channel,
        user_id: user,
        moderator_id: base + 3,
        reason: "prior strike".into(),
        source: StrikeSource::Interaction(id),
    };
    let previous = strikes::record(&pool, &make(base, base + 2, base + 10, base + 99))
        .await?
        .strike;
    strikes::record(&pool, &make(base + 100, base + 2, base + 11, base + 1)).await?;
    strikes::record(&pool, &make(base, base + 200, base + 12, base + 1)).await?;
    let current = strikes::record(&pool, &make(base, base + 2, base + 13, base + 1))
        .await?
        .strike;
    strikes::record(&pool, &make(base, base + 2, base + 14, base + 1)).await?;
    let global = rule(&pool, base, None, "strikes => null").await?;
    let scoped = rule(&pool, base, Some(vec![base + 1]), "strikes => null").await?;
    rule(&pool, base, Some(vec![]), "strikes => 'BAN'").await?;
    rule(&pool, base, Some(vec![base + 50]), "strikes => 'BAN'").await?;
    rule(&pool, base + 100, None, "strikes => 'BAN'").await?;
    let context = strike_actions::load_context(&pool, current.clone()).await?;
    assert_eq!(context.strike.id, current.id);
    assert_eq!(
        context
            .history
            .iter()
            .map(|strike| strike.id)
            .collect::<Vec<_>>(),
        [previous.id]
    );
    assert_eq!(
        context
            .actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<_>>(),
        [global, scoped]
    );
    cleanup(&pool, &[base, base + 100]).await?;
    pool.close().await;
    Ok(())
}

async fn cleanup(pool: &PgPool, guilds: &[i64]) -> Result<()> {
    for query in [
        "DELETE FROM strike_actions WHERE guild_id = ANY($1)",
        "DELETE FROM strikes WHERE guild_id = ANY($1)",
    ] {
        sqlx::query(query).bind(guilds).execute(pool).await?;
    }
    Ok(())
}

struct Captured {
    headers: String,
    body: Value,
}

fn assert_feedback(
    requests: &[Captured],
    channel: i64,
    user: i64,
    message: Option<i64>,
    count: usize,
) {
    let notices: Vec<_> = requests
        .iter()
        .filter(|request| request.headers.starts_with("post "))
        .collect();
    assert_eq!(notices.len(), 1);
    let notice = notices[0];
    assert!(
        notice
            .headers
            .starts_with(&format!("post /api/v10/channels/{channel}/messages "))
    );
    assert_eq!(
        notice.body["allowed_mentions"],
        json!({"parse":[], "users":[user.to_string()]})
    );
    let strikes = if count == 1 {
        "a strike".into()
    } else {
        format!("{count} strikes")
    };
    assert_eq!(
        notice.body["content"],
        format!("<@{user}>, you received {strikes}. Use /strikes to view the reasons.")
    );
    let deletes: Vec<_> = requests
        .iter()
        .filter(|request| request.headers.starts_with("delete /api/v10/channels/"))
        .collect();
    if let Some(message) = message {
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].headers.starts_with(&format!(
            "delete /api/v10/channels/{channel}/messages/{message} "
        )));
    } else {
        assert!(deletes.is_empty());
    }
}

#[tokio::test]
async fn strike_feedback_tolerates_deleted_messages_and_attempts_both_requests() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://localhost/unused")?;
    for (delete_status, notice_status, failures) in
        [(204, 200, 0), (404, 200, 0), (403, 200, 1), (403, 403, 2)]
    {
        let (http, requests) = discord_responses(2, delete_status, notice_status, 204)?;
        let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
        assert_eq!(
            moderation
                .notify_strikes(Id::new(200), Id::new(300), Some(Id::new(400)), 1)
                .await,
            failures
        );
        assert_feedback(&requests.join().unwrap(), 200, 300, Some(400), 1);
        assert_eq!(
            moderation
                .notify_strikes(Id::new(200), Id::new(300), Some(Id::new(400)), 0)
                .await,
            0
        );
    }
    Ok(())
}

#[tokio::test]
async fn own_notices_cannot_trigger_moderation() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://localhost/unused")?;
    let (http, requests) = discord_responses(0, 204, 200, 204)?;
    let moderation = Moderation::new(pool, http, jev(), Id::new(302));
    moderation
        .process_message(message_context(300, &[(1, "'STRIKE'"), (2, "'BAN'")]))
        .await?;
    moderation
        .process_message(message_context(400, &[(1, "null")]))
        .await?;
    let error = moderation
        .record_manual(&NewStrike {
            guild_id: 300,
            channel_id: 301,
            user_id: 302,
            moderator_id: 400,
            reason: "test".into(),
            source: StrikeSource::Interaction(500),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<StrikeCheckError>(),
        Some(StrikeCheckError::Protected)
    ));
    assert!(requests.join().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn hierarchy_blocks_both_strike_paths_before_storage_or_feedback() -> Result<()> {
    let pool =
        sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://localhost:1/unused")?;
    for (owner, roles, lookup_failure, protected) in [
        (900, vec!["13"], false, true),
        (900, vec!["11"], false, true),
        (300, vec!["12"], false, true),
        (900, vec!["99"], false, false),
        (900, vec!["12"], true, false),
    ] {
        let (http, requests) = discord_server(6, move |headers| {
            assert!(
                headers.starts_with("get "),
                "protected members must have no moderation side effects"
            );
            if lookup_failure && headers.starts_with("get /api/v10/guilds/298/members/300 ") {
                (403, json!({"code":50013, "message":"Missing Permissions"}))
            } else {
                hierarchy_response(headers, 298, 600, owner, &roles)
            }
        })?;
        let moderation = Moderation::new(pool.clone(), http, jev(), Id::new(600));
        let result = moderation
            .process_message(message_context(298, &[(1, "'STRIKE'"), (2, "'STRIKE'")]))
            .await;
        assert_eq!(result.is_ok(), protected);
        let mut interaction = crate::strikes::tests::payload();
        interaction["guild_id"] = json!("298");
        let response = strikes::handle(&moderation, &serde_json::from_value(interaction)?).await;
        assert!(response.contains("No strike was added"));
        if protected {
            assert!(response.contains("equal to or above mine"));
        } else {
            assert!(response.contains("couldn't verify"));
        }
        let requests = requests.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            requests
                .iter()
                .all(|request| request.headers.starts_with("get "))
        );
    }
    Ok(())
}

fn discord_responses(
    count: usize,
    delete_status: u16,
    notice_status: u16,
    removal_status: u16,
) -> Result<(Arc<Client>, thread::JoinHandle<Vec<Captured>>)> {
    discord_server(count, move |headers| {
        mutation_response(headers, [delete_status, notice_status, removal_status])
    })
}

fn mutation_response(headers: &str, statuses: [u16; 3]) -> (u16, Value) {
    let [delete_status, notice_status, removal_status] = statuses;
    let status = if headers.starts_with("post ") {
        notice_status
    } else if headers.starts_with("delete /api/v10/channels/") {
        delete_status
    } else {
        removal_status
    };
    let body = match status {
        200 | 204 => json!({}),
        404 => json!({"code":10008, "message":"Unknown Message"}),
        _ => json!({"code":50013, "message":"Missing Permissions"}),
    };
    (status, body)
}

pub(super) fn hierarchy_guild(guild: i64, owner: i64) -> Value {
    let role = |id: i64, position| {
        json!({
            "id":id.to_string(), "position":position, "name":"test", "color":0,
            "colors":{"primary_color":0}, "hoist":false, "managed":false,
            "mentionable":false, "permissions":"0", "flags":0,
        })
    };
    json!({
        "id":guild.to_string(), "owner_id":owner.to_string(), "roles":[role(guild, 0), role(11, 2), role(12, 1), role(13, 3)],
        "name":"test", "afk_timeout":60, "default_message_notifications":0,
        "explicit_content_filter":0, "features":[], "mfa_level":0,
        "preferred_locale":"en-US", "system_channel_flags":0,
        "premium_progress_bar_enabled":false, "verification_level":0, "nsfw_level":0,
    })
}

fn hierarchy_response(
    headers: &str,
    guild: i64,
    bot: i64,
    owner: i64,
    target_roles: &[&str],
) -> (u16, Value) {
    assert!(headers.starts_with(&format!("get /api/v10/guilds/{guild}")));
    let path = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    if !path.contains("/members/") {
        return (200, hierarchy_guild(guild, owner));
    }
    let user = path.rsplit('/').next().unwrap();
    (
        200,
        json!({
            "user":{"id":user, "username":"test", "discriminator":"0"},
            "roles":if user == bot.to_string() {vec!["11"]} else {target_roles.to_vec()},
            "deaf":false, "mute":false, "flags":0,
        }),
    )
}

fn discord_with_hierarchy(
    count: usize,
    statuses: [u16; 3],
    guild: i64,
    bot: i64,
) -> Result<(Arc<Client>, thread::JoinHandle<Vec<Captured>>)> {
    discord_server(count, move |headers| {
        if headers.starts_with("get ") {
            hierarchy_response(headers, guild, bot, 900, &["12"])
        } else {
            mutation_response(headers, statuses)
        }
    })
}

fn discord_server(
    count: usize,
    response: impl Fn(&str) -> (u16, Value) + Send + 'static,
) -> Result<(Arc<Client>, thread::JoinHandle<Vec<Captured>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let client = Client::builder()
        .token("test-token".into())
        .proxy(listener.local_addr()?.to_string(), true)
        .ratelimiter(None)
        .timeout(Duration::from_secs(5))
        .build();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing Discord request");
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
            let (status, response) = response(&headers);
            let response = if status == 204 {
                String::new()
            } else {
                response.to_string()
            };
            write!(stream,
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        ).unwrap();
            requests.push(Captured { headers, body });
        }
        requests
    });
    Ok((Arc::new(client), task))
}
