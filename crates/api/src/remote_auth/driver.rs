//! Async driver for QR login. Never logs the decrypted token.
use super::protocol::{RaAction, RaProtocol};
use crate::{ApiError, client_props};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at, sleep_until};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
    },
};

const URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const LOGIN_URL: &str = "https://discord.com/api/v9/users/@me/remote-auth/login";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrStatus {
    Qr(String),
    Scanned(String),
    Expired,
}

fn fail(e: impl ToString) -> ApiError {
    ApiError::RemoteAuth(e.to_string())
}

/// Runs the QR login flow and returns the account token.
pub async fn qr_login(status: mpsc::Sender<QrStatus>) -> Result<String, ApiError> {
    let r = run(&status).await;
    if r.is_err() {
        let _ = status.send(QrStatus::Expired).await;
    }
    r
}

async fn run(status: &mpsc::Sender<QrStatus>) -> Result<String, ApiError> {
    let mut req = URL.into_client_request().map_err(fail)?;
    let h = req.headers_mut();
    for (k, v) in client_props::base_headers() {
        h.insert(HeaderName::from_bytes(k.as_bytes()).map_err(fail)?, HeaderValue::from_str(&v).map_err(fail)?);
    }
    h.insert("Origin", HeaderValue::from_static("https://discord.com"));
    let (mut ws, _) = connect_async(req).await.map_err(fail)?;

    // RSA keygen is CPU-heavy; keep it off the async worker.
    let mut proto = tokio::task::spawn_blocking(RaProtocol::new).await.map_err(fail)?;
    let mut deadline = Instant::now() + Duration::from_secs(120); // replaced by hello's timeout_ms
    let mut hb: Option<tokio::time::Interval> = None;
    loop {
        let text = tokio::select! {
            _ = sleep_until(deadline) => return Err(fail("timed out")),
            _ = async { hb.as_mut().unwrap().tick().await }, if hb.is_some() => {
                ws.send(Message::text(r#"{"op":"heartbeat"}"#)).await.map_err(fail)?;
                continue;
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(t))) => t.to_string(),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Err(fail("socket closed")),
                Some(Ok(_)) => continue,
            },
        };
        if let Ok(Hello { op, timeout_ms: Some(ms) }) = serde_json::from_str(&text)
            && op == "hello"
        {
            deadline = Instant::now() + Duration::from_millis(ms);
        }
        for a in proto.on_text(&text).map_err(fail)? {
            match a {
                RaAction::Send(s) => ws.send(Message::text(s)).await.map_err(fail)?,
                RaAction::StartHeartbeat(d) => hb = Some(interval_at(Instant::now() + d, d)),
                RaAction::ShowQr(u) => status.send(QrStatus::Qr(u)).await.map_err(fail)?,
                RaAction::Scanned { username } => status.send(QrStatus::Scanned(username)).await.map_err(fail)?,
                RaAction::Cancelled => return Err(fail("cancelled on phone")),
                RaAction::Ticket(t) => {
                    let _ = ws.close(None).await;
                    return exchange_ticket(&proto, &t).await;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct Hello {
    op: String,
    timeout_ms: Option<u64>,
}

async fn exchange_ticket(proto: &RaProtocol, ticket: &str) -> Result<String, ApiError> {
    #[derive(Deserialize)]
    struct Resp {
        encrypted_token: String,
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in client_props::base_headers() {
        headers.insert(HeaderName::from_bytes(k.as_bytes()).map_err(fail)?, HeaderValue::from_str(&v).map_err(fail)?);
    }
    // No Authorization header: the user is not logged in yet.
    let resp = reqwest::Client::builder()
        .default_headers(headers)
        .build()?
        .post(LOGIN_URL)
        .json(&serde_json::json!({ "ticket": ticket }))
        .send()
        .await?;
    let code = resp.status();
    if !code.is_success() {
        return Err(ApiError::Http { status: code.as_u16(), body: resp.text().await.unwrap_or_default() });
    }
    let r: Resp = serde_json::from_slice(&resp.bytes().await?)?;
    proto.decrypt_token(&r.encrypted_token).map_err(fail)
}
