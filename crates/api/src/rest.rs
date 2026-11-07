use crate::{client_props::base_headers, id::Id, model::Message, ratelimit::RateLimiter};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;

const BASE: &str = "https://discord.com/api/v9";
const MAX_RETRIES: u32 = 3;

/// Where a join click happened; sent like the official client's invite button (X-Context-Properties).
#[derive(Debug, Clone, Copy)]
pub struct InviteContext {
    pub guild: Option<Id>,
    pub channel: Id,
    pub channel_type: u8,
    pub message: Id,
}

/// The guild joined via an invite (only what the app shows).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct JoinedGuild {
    pub id: Id,
    #[serde(default)]
    pub name: String,
}

#[derive(serde::Deserialize)]
struct InviteJoin {
    #[serde(default)]
    guild: Option<JoinedGuild>,
}

/// Invite codes go into the URL path: letters, digits and dashes only.
pub fn valid_invite_code(code: &str) -> bool {
    (2..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    /// QR login failed outside HTTP: protocol error, cancel, timeout, or socket closed.
    #[error("remote auth: {0}")]
    RemoteAuth(String),
    #[error("response over {0} bytes")]
    TooLarge(usize),
}

impl ApiError {
    /// Discord wants an hCaptcha solved before it accepts the request (rxcord can't show one).
    pub fn is_captcha(&self) -> bool {
        matches!(self, ApiError::Http { status: 400, body } if body.contains("captcha_key"))
    }
}

pub struct Rest {
    http: reqwest::Client,
    base: String,
    token: String,
    limiter: RateLimiter,
}

impl fmt::Debug for Rest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rest").field("base", &self.base).field("token", &"<redacted>").finish()
    }
}

impl Rest {
    pub fn new(token: String) -> Self {
        Self::with_base(token, BASE.to_string())
    }

    pub fn with_base(token: String, base: String) -> Self {
        Self { http: client_builder().build().expect("http client"), base, token, limiter: RateLimiter::default() }
    }

    /// Joins the guild of invite `code`. `None` guild: Discord didn't say which (the gateway will).
    pub async fn join_invite(&self, code: &str, session_id: &str, ctx: InviteContext) -> Result<Option<JoinedGuild>, ApiError> {
        if !valid_invite_code(code) {
            return Err(ApiError::Http { status: 404, body: "invalid invite code".into() });
        }
        let props = json!({
            "location": "Invite Button Embed",
            "location_guild_id": ctx.guild.map(|g| g.to_string()),
            "location_channel_id": ctx.channel.to_string(),
            "location_channel_type": ctx.channel_type,
            "location_message_id": ctx.message.to_string(),
        });
        let extra = [("X-Context-Properties", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, props.to_string()))];
        let body = self.send_with(Method::POST, "POST /invites".into(), format!("/invites/{code}"), Some(json!({"session_id": session_id})), &extra).await?;
        Ok(serde_json::from_slice::<InviteJoin>(&body)?.guild)
    }

    pub async fn messages(&self, channel: Id, before: Option<Id>, limit: u8) -> Result<Vec<Message>, ApiError> {
        let mut q = vec![("limit", limit.to_string())];
        if let Some(b) = before {
            q.push(("before", b.to_string()));
        }
        let path = format!("/channels/{channel}/messages?{}", encode(&q));
        self.json(Method::GET, format!("GET /channels/{channel}/messages"), path, None).await
    }

    pub async fn send_message(&self, channel: Id, content: &str, nonce: &str, reply_to: Option<Id>) -> Result<Message, ApiError> {
        // enforce_nonce: a retry with the same nonce returns the already-created message instead of a duplicate
        let mut body = json!({"content": content, "nonce": nonce, "enforce_nonce": true});
        if let Some(r) = reply_to {
            body["message_reference"] = json!({"message_id": r.to_string()});
        }
        let route = format!("POST /channels/{channel}/messages");
        self.json(Method::POST, route, format!("/channels/{channel}/messages"), Some(body)).await
    }

    pub async fn edit_message(&self, channel: Id, message: Id, content: &str) -> Result<Message, ApiError> {
        let route = format!("PATCH /channels/{channel}/messages");
        self.json(Method::PATCH, route, format!("/channels/{channel}/messages/{message}"), Some(json!({"content": content}))).await
    }

    pub async fn delete_message(&self, channel: Id, message: Id) -> Result<(), ApiError> {
        let route = format!("DELETE /channels/{channel}/messages");
        self.send(Method::DELETE, route, format!("/channels/{channel}/messages/{message}"), None).await.map(drop)
    }

    pub async fn ack(&self, channel: Id, message: Id) -> Result<(), ApiError> {
        let route = format!("POST /channels/{channel}/messages/ack");
        self.send(Method::POST, route, format!("/channels/{channel}/messages/{message}/ack"), Some(json!({"token": null}))).await.map(drop)
    }

    /// CDN fetch: no Authorization, only a User-Agent. A body over `max` bytes is an error.
    pub async fn get_bytes(&self, url: &str, max: usize) -> Result<Vec<u8>, ApiError> {
        let mut resp = self.http.get(url).header("User-Agent", crate::client_props::user_agent()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ApiError::Http { status: status.as_u16(), body: String::new() }); // untrusted body: not read
        }
        if resp.content_length().is_some_and(|n| n > max as u64) {
            return Err(ApiError::TooLarge(max));
        }
        let mut out = vec![];
        while let Some(chunk) = resp.chunk().await? {
            if out.len() + chunk.len() > max {
                return Err(ApiError::TooLarge(max));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn json<T: DeserializeOwned>(&self, m: Method, route: String, path: String, body: Option<Value>) -> Result<T, ApiError> {
        Ok(serde_json::from_slice(&self.send(m, route, path, body).await?)?)
    }

    async fn send(&self, m: Method, route: String, path: String, body: Option<Value>) -> Result<Vec<u8>, ApiError> {
        self.send_with(m, route, path, body, &[]).await
    }

    async fn send_with(
        &self,
        m: Method,
        route: String,
        path: String,
        body: Option<Value>,
        extra: &[(&str, String)],
    ) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}{path}", self.base);
        let mut retries = 0;
        loop {
            self.limiter.acquire(&route).await;
            let mut req = self.http.request(m.clone(), &url).header("Authorization", &self.token);
            for (k, v) in base_headers() {
                req = req.header(k, v);
            }
            for (k, v) in extra {
                req = req.header(*k, v);
            }
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status();
            let hdr = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
            self.limiter.update(
                &route,
                hdr("X-RateLimit-Remaining").and_then(|v| v.parse().ok()),
                hdr("X-RateLimit-Reset-After").and_then(|v| v.parse().ok()),
            );
            let retry_hdr = hdr("Retry-After").and_then(|v| v.parse::<f64>().ok());
            let bytes = resp.bytes().await?;
            match status.as_u16() {
                401 => return Err(ApiError::Unauthorized),
                429 if retries < MAX_RETRIES => {
                    retries += 1;
                    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                    let wait = v["retry_after"].as_f64().or(retry_hdr).unwrap_or(1.0);
                    self.limiter.on_429(&route, wait, v["global"].as_bool().unwrap_or(false));
                }
                _ if status.is_success() => return Ok(bytes.to_vec()),
                s => return Err(ApiError::Http { status: s, body: String::from_utf8_lossy(&bytes).into_owned() }),
            }
        }
    }
}

/// Shared timeouts so a dead network never hangs a request forever.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(30))
}

fn encode(q: &[(&str, String)]) -> String {
    q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

