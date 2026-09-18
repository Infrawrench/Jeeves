use super::*;

fn web() -> Web {
    let config =
        super::super::auth::Config::new("secret".into(), "bot".into(), "https://jeeves.example")
            .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/test")
        .unwrap();
    Web {
        auth: Manager::new(config, "client".into(), pool).unwrap(),
        states: Arc::default(),
    }
}

#[tokio::test]
async fn authorization_state_is_bound_to_the_browser_one_use_and_expires() {
    let web = web();
    let response = start(State(web.clone())).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let url = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        params["redirect_uri"],
        "https://jeeves.example/auth/twitch/callback"
    );
    assert_eq!(params["scope"], SCOPES.join(" "));
    assert_eq!(params["force_verify"], "true");
    let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    for flag in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(set_cookie.contains(flag));
    }
    let state = &params["state"];
    assert!(!consume_state(&web, &HeaderMap::new(), Some(state)).await);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{COOKIE}=wrong")).unwrap(),
    );
    assert!(!consume_state(&web, &headers, Some(state)).await);
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(set_cookie.split(';').next().unwrap()).unwrap(),
    );
    assert!(consume_state(&web, &headers, Some(state)).await);
    assert!(!consume_state(&web, &headers, Some(state)).await);
    web.states
        .lock()
        .await
        .insert(state.clone(), Instant::now() - Duration::from_secs(601));
    assert!(!consume_state(&web, &headers, Some(state)).await);
}

#[tokio::test]
async fn callback_without_valid_state_never_exchanges_a_code() {
    let response = callback(
        State(web()),
        Query(Callback {
            code: Some("sensitive-code".into()),
            state: None,
            error: None,
        }),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["Referrer-Policy"], "no-referrer");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
}
