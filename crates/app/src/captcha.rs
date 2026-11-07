//! Discord's hCaptcha, shown on a one-off local web page (rxcord's UI has no web engine).
//! The page is served on 127.0.0.1 under a random secret path and hands the solved token back here.
use rxcord_api::rest::CaptchaAnswer;
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How long the page waits for the user.
const WAIT: Duration = Duration::from_secs(300);
const MAX_REQUEST: usize = 64 * 1024;

/// What Discord sends with a captcha-required 400.
#[derive(Debug, Clone, PartialEq)]
pub struct Challenge {
    pub sitekey: String,
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

/// The challenge in a 400 body; None unless it's an hCaptcha one we can show.
pub fn parse_challenge(body: &str) -> Option<Challenge> {
    let v: Value = serde_json::from_str(body).ok()?;
    let s = |k: &str| v[k].as_str().map(str::to_string);
    if v["captcha_service"].as_str().is_some_and(|svc| svc != "hcaptcha") {
        return None;
    }
    let sitekey = s("captcha_sitekey").filter(|k| !k.is_empty() && k.len() < 100)?;
    Some(Challenge { sitekey, rqdata: s("captcha_rqdata"), rqtoken: s("captcha_rqtoken"), session_id: s("captcha_session_id") })
}

fn page(c: &Challenge) -> String {
    // JSON inside <script>: "</" would end the script element early.
    let cfg = serde_json::json!({"sitekey": c.sitekey, "rqdata": c.rqdata}).to_string().replace("</", "<\\/");
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>rxcord captcha</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{{font-family:sans-serif;background:#1e1f22;color:#ddd;display:flex;flex-direction:column;align-items:center;padding:12vh 16px}}</style>
</head><body>
<p>Discord wants a captcha before joining the server.</p>
<div id="c"></div>
<p id="s"></p>
<script>
const CFG = {cfg};
const say = t => document.getElementById("s").textContent = t;
const post = (what, body) => fetch(location.pathname + "/" + what, {{method: "POST", body}});
function go() {{
  const id = hcaptcha.render("c", {{
    sitekey: CFG.sitekey,
    theme: "dark",
    callback: t => {{ say("Sending to rxcord…"); post("token", t).then(() => say("Done: back to rxcord. You can close this tab.")); }},
    "error-callback": e => {{ say("Captcha error: " + e); post("error", String(e)); }},
  }});
  if (CFG.rqdata) hcaptcha.setData(id, {{rqdata: CFG.rqdata}});
}}
</script>
<script src="https://js.hcaptcha.com/1/api.js?render=explicit&onload=go" async defer></script>
</body></html>"#
    )
}

/// Serves the captcha page, calls `ready` with its URL (to open in the browser), and waits for the
/// solved token. Err: the page reported an error, or nobody solved it in time.
pub async fn solve(c: Challenge, ready: impl FnOnce(String)) -> Result<CaptchaAnswer, String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let secret: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    ready(format!("http://127.0.0.1:{port}/{secret}"));
    let serve = async {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            if let Some(done) = handle(stream, port, &secret, &c).await {
                return done;
            }
        }
    };
    let key = tokio::time::timeout(WAIT, serve).await.map_err(|_| "nobody solved the captcha in time".to_string())??;
    Ok(CaptchaAnswer { key, rqtoken: c.rqtoken, session_id: c.session_id })
}

/// One HTTP request. Some(..) ends the wait: the token, or the page's error.
async fn handle(mut s: TcpStream, port: u16, secret: &str, c: &Challenge) -> Option<Result<String, String>> {
    let req = read_request(&mut s).await?;
    let (head, body) = req.split_once("\r\n\r\n").unwrap_or((&req, ""));
    let mut lines = head.lines();
    let mut parts = lines.next()?.split(' ');
    let (method, path) = (parts.next()?, parts.next()?);
    // Only requests addressed to this exact host (no DNS rebinding from other sites).
    let host_ok = lines.any(|l| l.split_once(':').is_some_and(|(k, v)| k.eq_ignore_ascii_case("host") && v.trim() == format!("127.0.0.1:{port}")));
    let base = format!("/{secret}");
    let (status, ctype, out, done) = match (method, path) {
        _ if !host_ok => ("403 Forbidden", "text/plain", String::new(), None),
        ("GET", p) if p == base => ("200 OK", "text/html; charset=utf-8", page(c), None),
        ("POST", p) if p == format!("{base}/token") && valid_token(body) => ("200 OK", "text/plain", "ok".into(), Some(Ok(body.to_string()))),
        ("POST", p) if p == format!("{base}/error") => {
            let e: String = body.chars().filter(|c| !c.is_control()).take(100).collect();
            ("200 OK", "text/plain", "ok".into(), Some(Err(format!("the captcha couldn't run ({e})"))))
        }
        _ => ("404 Not Found", "text/plain", String::new(), None),
    };
    let resp = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{out}", out.len());
    let _ = s.write_all(resp.as_bytes()).await;
    done
}

/// hCaptcha tokens are long printable ASCII strings.
fn valid_token(t: &str) -> bool {
    (20..=20_000).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_graphic())
}

/// Headers plus a Content-Length body, capped; None for anything malformed or too big.
async fn read_request(s: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut chunk)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return None;
        }
        let text = String::from_utf8_lossy(&buf);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if body.len() >= len {
                break;
            }
        }
    }
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge() -> Challenge {
        parse_challenge(r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"site-1","captcha_service":"hcaptcha",
            "captcha_rqdata":"data</script>","captcha_rqtoken":"rq"}"#)
        .unwrap()
    }

    #[test]
    fn parses_and_escapes() {
        let c = challenge();
        assert_eq!((c.sitekey.as_str(), c.rqtoken.as_deref()), ("site-1", Some("rq")));
        assert!(parse_challenge(r#"{"captcha_sitekey":"k","captcha_service":"recaptcha"}"#).is_none());
        assert!(parse_challenge("{}").is_none());
        let p = page(&c);
        assert!(p.contains("\"site-1\"") && !p.contains("data</script>"), "rqdata can't close the script tag");
    }

    async fn http(url: &str, method: &str, path_suffix: &str, host: Option<&str>, body: &str) -> String {
        let addr = url.trim_start_matches("http://").split('/').next().unwrap().to_string();
        let path = format!("/{}{path_suffix}", url.rsplit('/').next().unwrap());
        let mut s = TcpStream::connect(&addr).await.unwrap();
        let host = host.unwrap_or(&addr);
        let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn serves_page_and_returns_token() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(solve(challenge(), move |u| drop(tx.send(u))));
        let url = rx.await.unwrap();
        assert!(http(&url, "GET", "", None, "").await.contains("site-1"));
        assert!(http(&url, "GET", "", Some("evil.example"), "").await.starts_with("HTTP/1.1 403"), "foreign Host refused");
        assert!(http(&url, "GET", "x", None, "").await.starts_with("HTTP/1.1 404"), "wrong secret");
        assert!(http(&url, "POST", "/token", None, "short").await.starts_with("HTTP/1.1 404"), "junk token ignored");
        let token = "P1_eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.token";
        http(&url, "POST", "/token", None, token).await;
        let a = task.await.unwrap().unwrap();
        assert_eq!((a.key.as_str(), a.rqtoken.as_deref()), (token, Some("rq")));
    }

    #[tokio::test]
    async fn page_error_ends_the_wait() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(solve(challenge(), move |u| drop(tx.send(u))));
        http(&rx.await.unwrap(), "POST", "/error", None, "invalid-data").await;
        assert!(task.await.unwrap().unwrap_err().contains("invalid-data"));
    }
}
