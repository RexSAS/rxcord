//! Async websocket driver around the sans-IO `Session`. Never logs the token or Send payloads.
use super::session::{Action, Session};
use crate::{backoff::backoff, client_props, event::Event, inflate::Inflater};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::time::{Instant, sleep, sleep_until};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
    },
};

const QUERY: &str = "/?encoding=json&v=9&compress=zlib-stream";

pub enum GatewayEvent {
    Event(Event),
    Status(ConnStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    AuthFailed,
}

enum Outcome {
    Reconnect,
    AuthFailed,
    Closed, // receiver dropped
}

/// Typed gateway loop; runs until `AuthFailed` or `tx` is closed.
pub async fn run_gateway(token: String, tx: Sender<GatewayEvent>) {
    run(token, tx, |_| {}).await;
}

/// Like `run_gateway` but forwards every raw op-0 frame as (event name, `d`) before typed parsing.
/// Frames are dropped (not blocked on) if `tx` is full; returns when `tx` closes.
pub async fn run_gateway_raw(token: String, tx: Sender<(String, Box<RawValue>)>) {
    #[derive(Deserialize)]
    struct Frame {
        op: u8,
        t: Option<String>,
        d: Option<Box<RawValue>>,
    }
    let (etx, mut erx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move { while erx.recv().await.is_some() {} });
    let raw_tx = tx.clone();
    let hook = move |text: &str| {
        if let Ok(Frame { op: 0, t: Some(t), d: Some(d) }) = serde_json::from_str(text) {
            let _ = raw_tx.try_send((t, d));
        }
    };
    tokio::select! {
        _ = run(token, etx, hook) => {}
        _ = tx.closed() => {}
    }
}

async fn emit(tx: &Sender<GatewayEvent>, ev: GatewayEvent) -> bool {
    tx.send(ev).await.is_ok()
}

async fn run(token: String, tx: Sender<GatewayEvent>, mut on_raw: impl FnMut(&str)) {
    let mut session = Session::new(token);
    let mut attempt = 0u32;
    loop {
        if !emit(&tx, GatewayEvent::Status(ConnStatus::Connecting)).await {
            return;
        }
        let mut got_dispatch = false;
        let outcome = match connect(&session).await {
            Some(ws) => {
                if !emit(&tx, GatewayEvent::Status(ConnStatus::Connected)).await {
                    return;
                }
                connection(ws, &mut session, &tx, &mut on_raw, &mut got_dispatch).await
            }
            None => Outcome::Reconnect,
        };
        if got_dispatch {
            attempt = 0;
        }
        match outcome {
            Outcome::Closed => return,
            Outcome::AuthFailed => {
                emit(&tx, GatewayEvent::Status(ConnStatus::AuthFailed)).await;
                return;
            }
            Outcome::Reconnect => {
                attempt += 1;
                if !emit(&tx, GatewayEvent::Status(ConnStatus::Reconnecting { attempt })).await {
                    return;
                }
                // Op 7 also backs off (attempt is 0 after a healthy session, so the delay is the minimum).
                sleep(backoff(attempt - 1, rand::random())).await;
            }
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(session: &Session) -> Option<Ws> {
    let url = match session.resume_url() {
        Some(u) => format!("{}{QUERY}", u.trim_end_matches('/')),
        None => format!("wss://gateway.discord.gg{QUERY}"),
    };
    let mut req = url.into_client_request().ok()?;
    for (k, v) in client_props::base_headers() {
        req.headers_mut().insert(HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(&v).ok()?);
    }
    match connect_async(req).await {
        Ok((ws, _)) => Some(ws),
        Err(e) => {
            tracing::warn!("gateway connect failed: {e}");
            None
        }
    }
}

async fn connection(
    mut ws: Ws,
    session: &mut Session,
    tx: &Sender<GatewayEvent>,
    on_raw: &mut impl FnMut(&str),
    got_dispatch: &mut bool,
) -> Outcome {
    let mut inflater = Inflater::new(); // fresh zlib context per connection
    let mut next_hb: Option<Instant> = None;
    loop {
        let actions = tokio::select! {
            _ = async { sleep_until(next_hb.unwrap()).await }, if next_hb.is_some() => {
                let interval = session.heartbeat_interval().unwrap_or(Duration::from_secs(40));
                next_hb = Some(next_hb.unwrap() + interval);
                session.on_heartbeat_due()
            }
            msg = ws.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => Ok(t.to_string()),
                    Some(Ok(Message::Binary(b))) => match inflater.push(&b) {
                        None => continue,
                        Some(Ok(s)) => Ok(s),
                        Some(Err(e)) => {
                            tracing::warn!("inflate failed: {e}");
                            return Outcome::Reconnect;
                        }
                    },
                    Some(Ok(Message::Close(f))) => Err(session.on_close(f.map(|f| u16::from(f.code)))),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => Err(session.on_close(None)),
                };
                match text {
                    Err(a) => vec![a],
                    Ok(text) => {
                        on_raw(&text);
                        let actions = session.on_text(&text);
                        if next_hb.is_none() {
                            // First frame is Hello, so the interval is fresh; jitter the first beat.
                            if let Some(i) = session.heartbeat_interval() {
                                next_hb = Some(Instant::now() + i.mul_f64(rand::random()));
                            }
                        }
                        actions
                    }
                }
            }
        };
        for a in actions {
            match a {
                Action::Send(s) => {
                    if ws.send(Message::text(s)).await.is_err() {
                        return Outcome::Reconnect;
                    }
                }
                Action::Dispatch(e) => {
                    *got_dispatch = true;
                    if !emit(tx, GatewayEvent::Event(e)).await {
                        return Outcome::Closed;
                    }
                }
                Action::Reconnect { .. } => {
                    let _ = ws.close(None).await;
                    return Outcome::Reconnect;
                }
                Action::AuthFailed => return Outcome::AuthFailed,
            }
        }
    }
}
