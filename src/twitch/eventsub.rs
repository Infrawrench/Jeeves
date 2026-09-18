use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered};
use serde_json::Value;
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
    time::{Instant, timeout, timeout_at},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use url::Url;

use super::{
    Notification,
    api::{Api, ApiError},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(super) type Channels = HashMap<String, mpsc::Sender<Notification>>;
type Key = (String, &'static str);
type Operation = BoxFuture<'static, (Key, Result<Option<String>>)>;
const ENDPOINT: &str = "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30";
const EVENTS: [&str; 4] = [
    "channel.chat.message",
    "channel.chat.message_delete",
    "channel.chat.clear",
    "channel.chat.clear_user_messages",
];

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Fatal(&'static str);

struct Welcome {
    id: String,
    keepalive: Duration,
}

pub async fn run(api: &Api, mut channels: watch::Receiver<Channels>) -> Result<()> {
    let mut delay = 1;
    loop {
        let started = Instant::now();
        let result = session(api, &mut channels, ENDPOINT).await;
        if let Err(error) = result {
            if error.is::<Fatal>()
                || error.downcast_ref::<ApiError>().is_some_and(|error| {
                    error.0.is_client_error() && error.0 != reqwest::StatusCode::TOO_MANY_REQUESTS
                })
            {
                return Err(error);
            }
            // Reconnect URLs contain session data. Do not log transport error details.
            tracing::warn!(
                retry_seconds = delay,
                "Twitch EventSub disconnected; reconnecting and resubscribing"
            );
        }
        if started.elapsed() > Duration::from_secs(60) {
            delay = 1;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(60);
    }
}

async fn connect(url: &str) -> Result<(Socket, Welcome)> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(256 * 1024))
        .max_frame_size(Some(256 * 1024));
    let (mut socket, _) = timeout(
        Duration::from_secs(15),
        connect_async_with_config(url, Some(config), false),
    )
    .await??;
    let welcome = timeout(Duration::from_secs(10), receive(&mut socket)).await??;
    Ok((socket, parse_welcome(&welcome)?))
}

fn parse_welcome(value: &Value) -> Result<Welcome> {
    ensure!(
        value["metadata"]["message_type"] == "session_welcome",
        "expected Twitch session welcome"
    );
    let session = &value["payload"]["session"];
    let id = session["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("missing Twitch session ID")?
        .to_owned();
    let seconds = session["keepalive_timeout_seconds"].as_u64().unwrap_or(30);
    ensure!(
        (10..=600).contains(&seconds),
        "invalid Twitch keepalive timeout"
    );
    Ok(Welcome {
        id,
        keepalive: Duration::from_secs(seconds),
    })
}

fn desired(api: &Api, channels: &Channels) -> HashSet<Key> {
    channels
        .keys()
        .flat_map(|channel| {
            EVENTS
                .into_iter()
                .filter(move |kind| channel != &api.user_id || *kind == "channel.chat.message")
                .map(move |kind| (channel.clone(), kind))
        })
        .collect()
}

async fn session(
    api: &Api,
    channels: &mut watch::Receiver<Channels>,
    endpoint: &str,
) -> Result<()> {
    let (mut socket, mut welcome) = connect(endpoint).await?;
    let mut active: HashMap<Key, String> = HashMap::new();
    let mut pending = HashSet::new();
    let mut failed = HashSet::new();
    let mut operations: FuturesUnordered<Operation> = FuturesUnordered::new();
    let mut retry = tokio::time::interval(Duration::from_secs(30));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut deadline = Instant::now() + welcome.keepalive;
    tracing::info!("Twitch EventSub connected");
    loop {
        // Reconcile subscriptions on this connection: joining one channel must not
        // disconnect existing channels and lose messages that Twitch cannot replay.
        let wanted = desired(api, &channels.borrow_and_update());
        let mut changes: Vec<_> = active
            .keys()
            .filter(|key| !wanted.contains(*key))
            .cloned()
            .collect();
        let mut additions: Vec<_> = wanted
            .difference(&active.keys().cloned().collect())
            .cloned()
            .collect();
        additions.sort_by_key(|(channel, _)| channel != &api.user_id);
        changes.extend(additions);
        for key in changes {
            if pending.len() >= 8 {
                break;
            }
            if pending.contains(&key) || failed.contains(&key) {
                continue;
            }
            pending.insert(key.clone());
            let id = active.get(&key).cloned();
            let api = api.clone();
            let session_id = welcome.id.clone();
            operations.push(Box::pin(async move {
                let result = match id {
                    Some(id) => api.unsubscribe(&id).await.map(|()| None),
                    None => api.subscribe(&session_id, &key.0, key.1).await.map(Some),
                };
                (key, result)
            }));
        }
        tokio::select! {
            change = channels.changed() => {
                change.context("Twitch channel manager stopped")?;
                failed.clear();
            },
            _ = retry.tick() => { failed.clear(); },
            Some((key, result)) = operations.next(), if !operations.is_empty() => {
                pending.remove(&key);
                match result {
                    Ok(Some(id)) => { active.insert(key, id); },
                    Ok(None) => { active.remove(&key); },
                    Err(error) if error.downcast_ref::<ApiError>().is_some_and(|error| {
                        matches!(error.0, reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS)
                    }) => {
                        tracing::warn!(channel_id = key.0, kind = key.1, ?error, "Twitch subscription unavailable; retrying in 30 seconds");
                        failed.insert(key);
                    },
                    Err(error) => return Err(error),
                }
            },
            value = timeout_at(deadline, receive(&mut socket)) => {
                let value = value??;
                deadline = Instant::now() + welcome.keepalive;
                match value["metadata"]["message_type"].as_str() {
                    Some("session_reconnect") => {
                        let url = value["payload"]["session"]["reconnect_url"]
                            .as_str().context("missing Twitch reconnect URL")?;
                        validate_reconnect_url(url)?;
                        let reconnect = connect(url);
                        tokio::pin!(reconnect);
                        let (replacement, next_welcome) = loop {
                            tokio::select! {
                                result = &mut reconnect => break result?,
                                value = receive(&mut socket) => route(value?, &channels.borrow())?,
                            }
                        };
                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                        socket = replacement;
                        welcome = next_welcome;
                        deadline = Instant::now() + welcome.keepalive;
                        // Existing IDs transfer; new requests use the replacement session ID.
                    },
                    Some("revocation") => {
                        let id = value["payload"]["subscription"]["id"].as_str()
                            .context("missing revoked subscription ID")?;
                        if let Some(key) = active.iter().find_map(|(key, value)| (value == id).then(|| key.clone())) {
                            active.remove(&key);
                            tracing::warn!(channel_id = key.0, kind = key.1, "Twitch revoked a channel subscription; check moderator access");
                            failed.insert(key);
                        }
                    },
                    _ => route(value, &channels.borrow())?,
                }
            },
        }
    }
}

pub(super) fn validate_reconnect_url(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    ensure!(
        url.scheme() == "wss"
            && url.host_str() == Some("eventsub.wss.twitch.tv")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none_or(|port| port == 443)
            && url.fragment().is_none(),
        "invalid Twitch reconnect endpoint"
    );
    Ok(())
}

fn route(value: Value, channels: &Channels) -> Result<()> {
    match value["metadata"]["message_type"].as_str() {
        Some("notification") => {
            let notification: Notification = serde_json::from_value(value)?;
            if !EVENTS.contains(&notification.payload.subscription.kind.as_str()) {
                return Ok(());
            }
            let channel = notification.payload.event["broadcaster_user_id"]
                .as_str()
                .context("missing Twitch channel")?;
            // Late events may arrive while a removed subscription is being deleted.
            let Some(sender) = channels.get(channel) else {
                return Ok(());
            };
            // Never let AI/database latency prevent WebSocket keepalives. Stop visibly
            // on overload instead of accumulating unbounded work or dropping it silently.
            sender.try_send(notification).map_err(|_| {
                Fatal("Twitch channel queue is full or disconnected; moderation cannot keep up")
            })?;
        }
        Some("revocation") => {
            return Err(Fatal(
                "Twitch revoked a chat subscription; check the bot authorization and restart",
            )
            .into());
        }
        Some("session_keepalive") => {}
        _ => anyhow::bail!("unexpected Twitch EventSub message"),
    }
    Ok(())
}

async fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket.next().await.context("Twitch WebSocket closed")?? {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) => socket.flush().await?, // tungstenite queues the matching Pong.
            Message::Pong(_) => {}
            Message::Close(_) => anyhow::bail!("Twitch WebSocket closed"),
            _ => anyhow::bail!("unexpected Twitch WebSocket frame"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn live_channels_subscribe_and_unsubscribe_without_reconnecting() -> Result<()> {
        use crate::twitch::tests::observed_server;
        let responses = (0..5)
            .map(|id| (202, json!({"data": [{"id": format!("sub-{id}")}]})))
            .chain((0..4).map(|_| (204, Value::Null)))
            .collect();
        let (base, http, mut requests) = observed_server(responses);
        let api = Api::for_test(base);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("ws://{}", listener.local_addr()?);
        let (events, mut input) = mpsc::channel::<Value>(10);
        let websocket = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"metadata": {"message_type": "session_welcome"},
                "payload": {"session": {"id": "session", "keepalive_timeout_seconds": 30}}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while let Some(value) = input.recv().await {
                socket
                    .send(Message::Text(value.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let (lobby_tx, mut lobby_rx) = mpsc::channel(10);
        let (customer_tx, mut customer_rx) = mpsc::channel(10);
        let lobby = HashMap::from([("300".to_owned(), lobby_tx)]);
        let (updates, mut channels) = watch::channel(lobby.clone());
        let connection = tokio::spawn(async move { session(&api, &mut channels, &endpoint).await });
        let request = timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        assert_eq!(request.body["condition"]["broadcaster_user_id"], "300");
        assert_eq!(request.body["type"], "channel.chat.message");
        let mut joined = lobby.clone();
        joined.insert("100".into(), customer_tx);
        updates.send_replace(joined);
        let mut kinds = HashSet::new();
        for _ in 0..4 {
            let request = timeout(Duration::from_secs(5), requests.recv())
                .await?
                .unwrap();
            assert_eq!(request.body["condition"]["broadcaster_user_id"], "100");
            assert_eq!(request.body["transport"]["session_id"], "session");
            kinds.insert(request.body["type"].as_str().unwrap().to_owned());
        }
        assert_eq!(kinds, EVENTS.map(str::to_owned).into_iter().collect());
        let notification = |channel: &str| {
            json!({"metadata": {"message_type": "notification",
            "message_id": "event", "message_timestamp": "2026-09-18T00:00:00Z"},
            "payload": {"subscription": {"type": "channel.chat.message"},
                "event": {"broadcaster_user_id": channel}}})
        };
        events.send(notification("100")).await?;
        assert!(
            timeout(Duration::from_secs(5), customer_rx.recv())
                .await?
                .is_some()
        );
        updates.send_replace(lobby);
        let mut deleted = HashSet::new();
        for _ in 0..4 {
            let request = timeout(Duration::from_secs(5), requests.recv())
                .await?
                .unwrap();
            assert!(
                request
                    .path
                    .starts_with("DELETE /eventsub/subscriptions?id=")
            );
            deleted.insert(request.path);
        }
        assert_eq!(
            deleted,
            (1..5)
                .map(|id| format!("DELETE /eventsub/subscriptions?id=sub-{id} HTTP/1.1"))
                .collect()
        );
        events.send(notification("100")).await?; // Late removed-channel event is harmless.
        events.send(notification("300")).await?;
        assert!(
            timeout(Duration::from_secs(5), lobby_rx.recv())
                .await?
                .is_some()
        );
        assert!(customer_rx.try_recv().is_err());
        assert!(!connection.is_finished());
        assert_eq!(http.join().unwrap().len(), 9);
        connection.abort();
        websocket.abort();
        Ok(())
    }

    #[tokio::test]
    async fn websocket_ping_is_answered_before_reading_the_next_event() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Ping(b"keepalive".to_vec().into()))
                .await
                .unwrap();
            let pong = timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(pong, Message::Pong(b"keepalive".to_vec().into()));
            socket
                .send(Message::Text(
                    json!({"metadata": {"message_type": "session_keepalive"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
        let value = timeout(Duration::from_secs(3), receive(&mut socket)).await??;
        assert_eq!(value["metadata"]["message_type"], "session_keepalive");
        server.await?;
        Ok(())
    }

    #[test]
    fn reconnect_urls_cannot_redirect_session_data() {
        assert!(validate_reconnect_url("wss://eventsub.wss.twitch.tv/ws?reconnect=abc").is_ok());
        for url in [
            "ws://eventsub.wss.twitch.tv/ws",
            "wss://evil.test/ws",
            "wss://eventsub.wss.twitch.tv.evil.test/ws",
            "wss://token@eventsub.wss.twitch.tv/ws",
            "wss://eventsub.wss.twitch.tv:444/ws",
        ] {
            assert!(validate_reconnect_url(url).is_err());
        }
    }

    #[test]
    fn welcomes_validate_identity_and_keepalive() {
        let mut value = json!({"metadata": {"message_type": "session_welcome"}, "payload": {"session": {"id": "session", "keepalive_timeout_seconds": 30}}});
        assert_eq!(
            parse_welcome(&value).unwrap().keepalive,
            Duration::from_secs(30)
        );
        value["payload"]["session"]["keepalive_timeout_seconds"] = json!(0);
        assert!(parse_welcome(&value).is_err());
    }

    #[tokio::test]
    async fn routing_is_scoped_and_overload_is_fatal() {
        let (tx, mut rx) = mpsc::channel(1);
        let channels = HashMap::from([("123".into(), tx)]);
        let event = json!({"metadata": {"message_type": "notification", "message_id": "event", "message_timestamp": "2026-09-18T00:00:00Z"}, "payload": {"subscription": {"type": "channel.chat.clear"}, "event": {"broadcaster_user_id": "123"}}});
        route(event.clone(), &channels).unwrap();
        assert!(route(event.clone(), &channels).unwrap_err().is::<Fatal>());
        assert_eq!(
            rx.recv().await.unwrap().payload.event["broadcaster_user_id"],
            "123"
        );
        let mut other = event;
        other["payload"]["event"]["broadcaster_user_id"] = json!("456");
        assert!(route(other, &channels).is_ok());
        assert!(
            route(
                json!({"metadata": {"message_type": "revocation"}}),
                &channels
            )
            .unwrap_err()
            .is::<Fatal>()
        );
    }
}
