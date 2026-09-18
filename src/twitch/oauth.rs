//! Hosted bot authorization. Broadcasters enroll through chat, without running a server.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use tokio::{
    sync::{Mutex, watch},
    time::Instant,
};

use super::{api::SCOPES, auth::Manager};

const COOKIE: &str = "__Host-jeeves_oauth";
const MAX_STATES: usize = 1024;

#[derive(Clone)]
struct Web {
    auth: Arc<Manager>,
    states: Arc<Mutex<HashMap<String, Instant>>>,
}

pub async fn serve(auth: Arc<Manager>, mut stop: watch::Receiver<bool>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    let web = Web {
        auth,
        states: Arc::default(),
    };
    tracing::info!("Jeeves HTTP service listening on port 8080");
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move {
            super::stopped(&mut stop).await;
        })
        .await?;
    Ok(())
}

fn router(web: Web) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/favicon.png", get(favicon_png))
        .route("/favicon.ico", get(favicon_ico))
        .route("/healthz", get(health))
        .route("/auth/twitch", get(start))
        .route("/auth/twitch/callback", get(callback))
        .fallback(|| async {
            page(
                StatusCode::NOT_FOUND,
                "Page not found",
                "<p><a href='/'>Return to Jeeves</a></p>",
            )
        })
        .with_state(web)
}

async fn favicon_png() -> Response {
    favicon("image/png", include_bytes!("assets/favicon.png"))
}

async fn favicon_ico() -> Response {
    favicon("image/x-icon", include_bytes!("assets/favicon.ico"))
}

fn favicon(content_type: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

async fn health(State(web): State<Web>) -> StatusCode {
    match sqlx::query("SELECT 1").execute(&web.auth.pool).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn home(State(web): State<Web>) -> Response {
    let login = &web.auth.config.bot_login; // Configuration accepts only lowercase login characters.
    // The hosted Jeeves application and permissions from the README's Discord invite.
    let discord_invite = "https://discord.com/oauth2/authorize?client_id=1550283057125392566&amp;scope=bot%20applications.commands&amp;permissions=268512262&amp;integration_type=0";
    page(
        StatusCode::OK,
        "Discord and Twitch moderation",
        &format!(
            r##"<p class="eyebrow">DISCORD &amp; TWITCH MODERATION</p>
<h1>Your rules.<br>A calmer chat.</h1>
<p class="intro">Moderation rules in plain English for your Discord server and Twitch channel. Jeeves keeps track of strikes and applies the actions you choose.</p>
<nav class="actions" aria-label="Add Jeeves">
  <a class="button" href="{discord_invite}">Invite to Discord</a>
  <a class="button secondary" href="#twitch">Set up Twitch</a>
</nav>
<p>Everything runs on our server. You don’t need to install anything or keep this page open.</p>
<section id="discord" aria-labelledby="discord-title">
  <p class="section-label">DISCORD</p>
  <h2 id="discord-title">Add Jeeves to your server</h2>
  <p>Describe what belongs in your community, and let Jeeves evaluate messages and image attachments against your rules. Record strikes, kick or ban members, and give or revoke roles based on the conditions you set.</p>
  <ol>
    <li><a href="{discord_invite}">Invite Jeeves</a> and choose your Discord server.</li>
    <li>Give Jeeves access to the channels you want it to moderate. Place its role above the members it should moderate and the roles it should manage.</li>
    <li>As a server administrator, add a rule in plain English:
      <code>/addaction question:Strike users who post unsolicited advertising.</code>
      Add an escalation rule to choose what happens after repeated violations:
      <code>/addaction question:Ban a user when they have at least five strikes.</code>
    </li>
  </ol>
  <p>Use <code>/manageactions</code> to review or remove rules and <code>/managestrikes</code> to manage a member’s strikes. Members can privately check their own record with <code>/strikes</code>.</p>
  <p>There are no default moderation rules or automatic strike penalties. Each Discord server and Twitch channel keeps its own rules and strikes.</p>
</section>
<section id="twitch" aria-labelledby="twitch-title">
  <p class="section-label">TWITCH</p>
  <h2 id="twitch-title">Add Jeeves to your channel</h2>
  <p>Keep chat in line with message deletion, strikes, timeouts, and bans while you stream.</p>
  <ol>
    <li>In your own Twitch chat, make the bot a moderator:<code>/mod {login}</code></li>
    <li>Open <a href="https://www.twitch.tv/{login}">{login}’s chat</a> and send <code>!join</code></li>
    <li>Back in your channel, add a rule in plain English:<code>!addaction Time out users for ten minutes for targeted harassment.</code></li>
  </ol>
  <h3>Example Twitch rules</h3>
  <code>!addaction Strike users who post unsolicited advertising.</code>
  <code>!addaction Ban users when they have at least five strikes.</code>
  <p>Broadcasters and moderators can manage rules. Use <code>!jeeves help</code> in your channel to see the commands.</p>
  <p>To stop, send <code>!leave</code> in the bot’s chat. Your rules and strikes are kept if you rejoin.</p>
</section>
<footer>
  <p>Chat messages, Discord image attachments, and rule descriptions are processed by our AI providers to apply your rules. Jeeves keeps recent chat history and persistent strikes.</p>
  <a href="/auth/twitch">Twitch bot account setup</a><span> · Only for the operator of {login}</span>
</footer>"##
        ),
    )
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><title>{title} · Jeeves</title><link rel="icon" type="image/png" sizes="64x64" href="/favicon.png"><link rel="icon" type="image/x-icon" sizes="16x16 32x32 48x48" href="/favicon.ico"><style>
:root{{color-scheme:dark;font-family:ui-sans-serif,system-ui,sans-serif;background:#111016;color:#eeeaf7}}*{{box-sizing:border-box}}body{{margin:0}}main{{max-width:800px;margin:auto;padding:72px 24px}}.brand{{font-size:20px;font-weight:750;color:#d9c8ff;text-decoration:none}}h1{{font-size:clamp(44px,8vw,76px);line-height:1.04;letter-spacing:-.05em;margin:28px 0}}h2{{font-size:24px;letter-spacing:-.02em}}h3{{font-size:20px;margin-top:32px}}p,li{{line-height:1.7;color:#cac4d5}}.eyebrow,.section-label{{font-size:12px;letter-spacing:.18em;color:#bc9cff}}.eyebrow{{margin-top:56px}}.intro{{font-size:20px;max-width:600px}}.actions{{display:flex;flex-wrap:wrap;gap:12px;margin:28px 0}}.button{{display:inline-flex;align-items:center;justify-content:center;min-height:48px;padding:12px 20px;border:1px solid #c9acff;border-radius:8px;background:#c9acff;color:#20152e;font-weight:700;text-decoration:none}}.button:hover{{background:#decaff;border-color:#decaff}}.button.secondary{{background:transparent;color:#c9acff;border-color:#645275}}.button.secondary:hover{{background:#26202f}}a:focus-visible{{outline:3px solid #f1e6ff;outline-offset:4px}}section{{border-top:1px solid #37313f;margin-top:42px;padding-top:24px;scroll-margin-top:24px}}li{{padding:8px 0}}a{{color:#c9acff}}code{{font-family:ui-monospace,monospace;font-size:14px;background:#26202f;padding:3px 7px;border-radius:5px;overflow-wrap:anywhere}}li>code,section>code{{display:block;margin:12px 0;padding:14px 16px}}footer{{margin-top:48px;font-size:13px;color:#91899e}}footer p{{color:#91899e}}
</style></head><body><main><a class="brand" href="/">Jeeves</a>{body}</main></body></html>"#
    );
    let mut response = (status, Html(html)).into_response();
    secure_headers(&mut response);
    response
}

fn secure_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("Referrer-Policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("Content-Security-Policy", HeaderValue::from_static("default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
}

async fn start(State(web): State<Web>) -> Response {
    let mut bytes = [0_u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again",
            "<p>Authorization is temporarily unavailable.</p>",
        );
    }
    let nonce = URL_SAFE_NO_PAD.encode(bytes);
    {
        let mut states = web.states.lock().await;
        states.retain(|_, created| created.elapsed() < Duration::from_secs(600));
        if states.len() >= MAX_STATES {
            return page(
                StatusCode::TOO_MANY_REQUESTS,
                "Try again shortly",
                "<p>Please wait a few minutes before starting authorization again.</p>",
            );
        }
        states.insert(nonce.clone(), Instant::now());
    }
    let mut url =
        url::Url::parse("https://id.twitch.tv/oauth2/authorize").expect("constant OAuth URL");
    url.query_pairs_mut().extend_pairs([
        ("client_id", web.auth.client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", &web.auth.config.redirect_uri()),
        ("scope", &SCOPES.join(" ")),
        ("state", &nonce),
        ("force_verify", "true"),
    ]);
    let mut response = Redirect::to(url.as_str()).into_response();
    secure_headers(&mut response);
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&nonce, 600));
    response
}

fn cookie(value: &str, seconds: u32) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={seconds}"
    ))
    .expect("cookie contains only base64url or an empty value")
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn consume_state(web: &Web, headers: &HeaderMap, state: Option<&str>) -> bool {
    let Some(state) = state.filter(|state| state.len() == 43) else {
        return false;
    };
    let cookie_matches = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .any(|(name, value)| name == COOKIE && value == state);
    if !cookie_matches {
        return false;
    }
    web.states
        .lock()
        .await
        .remove(state)
        .is_some_and(|created| created.elapsed() < Duration::from_secs(600))
}

async fn callback(
    State(web): State<Web>,
    Query(query): Query<Callback>,
    headers: HeaderMap,
) -> Response {
    let mut response = if !consume_state(&web, &headers, query.state.as_deref()).await {
        page(
            StatusCode::BAD_REQUEST,
            "Start authorization again",
            "<h1>This link has expired.</h1><p>Please <a href='/auth/twitch'>start again</a> in the same browser. Authorization links can only be used once.</p>",
        )
    } else if query.error.is_some() {
        page(
            StatusCode::BAD_REQUEST,
            "Authorization cancelled",
            "<h1>Nothing was changed.</h1><p>You can <a href='/auth/twitch'>try again</a> whenever you’re ready.</p>",
        )
    } else if let Some(code) = query
        .code
        .filter(|code| !code.is_empty() && code.len() <= 2048)
    {
        match web.auth.exchange_code(&code).await {
            Ok(()) => page(
                StatusCode::OK,
                "Bot authorized",
                "<h1>Jeeves is ready.</h1><p>The bot account has been authorized. It will connect to Twitch shortly and renew its access automatically.</p><p><a href='/'>Add Jeeves to your channel</a></p>",
            ),
            Err(error) => {
                tracing::warn!(?error, "Twitch authorization could not be completed");
                page(
                    StatusCode::BAD_REQUEST,
                    "Authorization failed",
                    &format!(
                        "<h1>Let’s try that again.</h1><p>Sign in as <strong>{}</strong> and allow all the requested permissions. If that account is already selected, please try again shortly.</p><a href='/auth/twitch'>Authorize the bot account</a>",
                        web.auth.config.bot_login
                    ),
                )
            }
        }
    } else {
        page(
            StatusCode::BAD_REQUEST,
            "Missing authorization",
            "<p>Please <a href='/auth/twitch'>start authorization again</a>.</p>",
        )
    };
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie("", 0));
    response
}

#[cfg(test)]
mod tests;
