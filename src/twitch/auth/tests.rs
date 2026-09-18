use super::*;
use crate::twitch::{
    Action,
    api::{Api, SCOPES},
    tests::{message, server},
};
use serde_json::{Value, json};

fn grant() -> Value {
    json!({"access_token": "renewed-access", "refresh_token": "rotated-refresh", "token_type": "bearer"})
}

fn identity(client: &str) -> Value {
    json!({"client_id": client, "user_id": "300", "login": "bot", "scopes": SCOPES})
}

async fn manager(client: &str, base: &str) -> Result<Arc<Manager>> {
    let pool = crate::db::connect(
        crate::config::database_options(&std::env::var("TEST_DATABASE_URL")?)?,
        5,
    )
    .await?;
    sqlx::query("DELETE FROM twitch_oauth_tokens WHERE client_id = $1")
        .bind(client)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO twitch_oauth_tokens (client_id, bot_user_id, bot_login, access_token, refresh_token) VALUES ($1, '300', 'bot', 'old-access', 'old+refresh&token')")
        .bind(client).execute(&pool).await?;
    let mut auth = Manager::new(
        Config::new("secret".into(), "bot".into(), "https://jeeves.example")?,
        client.into(),
        pool,
    )?;
    let inner = Arc::get_mut(&mut auth).unwrap();
    inner.token_url = format!("{base}/token");
    inner.validate_url = format!("{base}/validate");
    Ok(auth)
}

#[test]
fn configuration_requires_a_safe_https_origin_and_login() {
    for origin in [
        "http://example.com",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/?query",
        "https://example.com/#fragment",
    ] {
        assert!(Config::new("secret".into(), "bot".into(), origin).is_err());
    }
    assert!(Config::new("secret".into(), "<bot>".into(), "https://example.com").is_err());
    assert_eq!(
        Config::new("secret".into(), "bot".into(), "https://example.com")
            .unwrap()
            .redirect_uri(),
        "https://example.com/auth/twitch/callback"
    );
    assert!(bearer("credential").unwrap().is_sensitive());
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn concurrent_refreshes_rotate_once_and_survive_a_new_manager() -> Result<()> {
    let client = "oauth-concurrent";
    let (base, requests) = server(vec![(200, grant()), (200, identity(client))]);
    let auth = manager(client, &base).await?;
    let (first, second) = tokio::join!(
        auth.refresh_after("old-access"),
        auth.refresh_after("old-access")
    );
    assert_eq!(first?, "renewed-access");
    assert_eq!(second?, "renewed-access");
    let stored = auth.load().await?.unwrap();
    assert_eq!(stored.refresh_token, "rotated-refresh");
    let restarted = Manager::new(auth.config.clone(), client.into(), auth.pool.clone())?;
    assert_eq!(restarted.access_token().await?, "renewed-access");
    let requests = requests.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["refresh_token"], "old+refresh&token");
    assert_eq!(requests[0].body["grant_type"], "refresh_token");
    auth.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn authorization_rejects_wrong_account_or_missing_permissions_without_overwriting()
-> Result<()> {
    let client = "oauth-reject";
    let mut wrong_bot = identity(client);
    wrong_bot["login"] = json!("broadcaster");
    let mut missing_scope = identity(client);
    missing_scope["scopes"] = json!(["user:read:chat"]);
    let (base, requests) = server(vec![
        (200, grant()),
        (200, wrong_bot),
        (200, grant()),
        (200, missing_scope),
        (400, json!({"message":"upstream-sensitive-value"})),
    ]);
    let auth = manager(client, &base).await?;
    assert!(auth.exchange_code("code").await.is_err());
    assert!(auth.exchange_code("code2").await.is_err());
    let error = auth.refresh_after("old-access").await.unwrap_err();
    assert!(error.is::<AuthorizationRequired>());
    assert!(!format!("{error:?}").contains("upstream-sensitive-value"));
    assert_eq!(auth.access_token().await?, "old-access");
    let requests = requests.join().unwrap();
    assert_eq!(
        requests[0].body["redirect_uri"],
        "https://jeeves.example/auth/twitch/callback"
    );
    auth.pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn authorization_persists_and_moderation_retries_only_a_401_once() -> Result<()> {
    let client = "oauth-api-retry";
    let (base, requests) = server(vec![
        (200, grant()),
        (200, identity(client)),
        (401, json!({})),
        (200, grant()),
        (200, identity(client)),
        (204, json!({})),
        (500, json!({})),
    ]);
    let auth = manager(client, &base).await?;
    auth.exchange_code("code").await?;
    assert_eq!(auth.access_token().await?, "renewed-access");
    let api = Api::for_test_managed(base, auth.clone());
    api.apply(&message(), &Action::Timeout(600), "test").await?;
    let error = api
        .apply(&message(), &Action::Ban, "test")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ApiError>().unwrap().0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let requests = requests.join().unwrap();
    assert_eq!(requests.len(), 7);
    assert_eq!(requests[2].path, requests[5].path);
    assert_eq!(requests[2].body, requests[5].body);
    assert!(requests[5].headers.contains("Bearer renewed-access"));
    auth.pool.close().await;
    Ok(())
}
