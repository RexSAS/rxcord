use crate::{client_props::base_headers, id::Id, model::Message, ratelimit::RateLimiter};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt;

const BASE: &str = "https://discord.com/api/v9";
const MAX_RETRIES: u32 = 3;

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
        Self { http: reqwest::Client::new(), base, token, limiter: RateLimiter::default() }
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
        let mut body = json!({"content": content, "nonce": nonce});
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

    /// CDN fetch: no Authorization, only a User-Agent.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, ApiError> {
        let resp = self.http.get(url).header("User-Agent", crate::client_props::user_agent()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ApiError::Http { status: status.as_u16(), body: resp.text().await.unwrap_or_default() });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn json<T: DeserializeOwned>(&self, m: Method, route: String, path: String, body: Option<Value>) -> Result<T, ApiError> {
        Ok(serde_json::from_slice(&self.send(m, route, path, body).await?)?)
    }

    async fn send(&self, m: Method, route: String, path: String, body: Option<Value>) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}{path}", self.base);
        let mut retries = 0;
        loop {
            self.limiter.acquire(&route).await;
            let mut req = self.http.request(m.clone(), &url).header("Authorization", &self.token);
            for (k, v) in base_headers() {
                req = req.header(k, v);
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

fn encode(q: &[(&str, String)]) -> String {
    q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

