use rxcord_api::id::Id;
use rxcord_api::rest::{ApiError, Rest};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MSG: &str = r#"{"id":"2","channel_id":"1","author":{"id":"3"},"content":"hi","timestamp":"2026-11-07T10:01:00.000000+00:00"}"#;

fn msg() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(MSG, "application/json")
}

#[test]
fn rest_builds_and_redacts_token() {
    let r = Rest::new("SECRET".into());
    assert!(!format!("{r:?}").contains("SECRET"));
}

#[tokio::test]
async fn send_message_body() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/channels/1/messages"))
        .and(header("Authorization", "tok"))
        .and(body_json(json!({"content":"hi","nonce":"n1","enforce_nonce":true,"message_reference":{"message_id":"9"}})))
        .respond_with(msg())
        .expect(1)
        .mount(&s)
        .await;
    let r = Rest::with_base("tok".into(), s.uri());
    let m = r.send_message(Id(1), "hi", "n1", Some(Id(9))).await.unwrap();
    assert_eq!(m.content, "hi");
}

#[tokio::test]
async fn unauthorized_maps() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&s).await;
    let r = Rest::with_base("tok".into(), s.uri());
    assert!(matches!(r.messages(Id(1), None, 50).await, Err(ApiError::Unauthorized)));
}

#[tokio::test]
async fn retries_after_429() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"retry_after":0.01,"global":false})))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).mount(&s).await;
    let r = Rest::with_base("tok".into(), s.uri());
    r.delete_message(Id(1), Id(2)).await.unwrap();
}

#[tokio::test]
async fn gives_up_after_three_retries() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"retry_after":0.01,"global":false})))
        .expect(4)
        .mount(&s)
        .await;
    let r = Rest::with_base("tok".into(), s.uri());
    assert!(matches!(r.delete_message(Id(1), Id(2)).await, Err(ApiError::Http { status: 429, .. })));
}

#[tokio::test]
async fn get_bytes_caps_body() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).and(path("/a.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 100])).mount(&s).await;
    let r = Rest::new(String::new());
    let url = format!("{}/a.png", s.uri());
    assert_eq!(r.get_bytes(&url, 100).await.unwrap().len(), 100);
    assert!(matches!(r.get_bytes(&url, 99).await, Err(ApiError::TooLarge(99))));
}

#[tokio::test]
async fn join_invite_sends_session_and_context() {
    use base64::Engine;
    use rxcord_api::rest::{InviteContext, JoinedGuild};
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/invites/abc-1"))
        .and(body_json(json!({"session_id": "sess"})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"code":"abc-1","guild":{"id":"77","name":"Cool"}}"#, "application/json"))
        .expect(1)
        .mount(&s)
        .await;
    let r = Rest::with_base("tok".into(), s.uri());
    let ctx = InviteContext { guild: None, channel: Id(5), channel_type: 1, message: Id(6) };
    let g = r.join_invite("abc-1", "sess", ctx, None).await.unwrap();
    assert_eq!(g, Some(JoinedGuild { id: Id(77), name: "Cool".into() }));
    let req = &s.received_requests().await.unwrap()[0];
    let props = req.headers.get("X-Context-Properties").unwrap().to_str().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(props).unwrap()).unwrap();
    assert_eq!(v, json!({"location": "Invite Button Embed", "location_guild_id": null, "location_channel_id": "5",
        "location_channel_type": 1, "location_message_id": "6"}));
    // a code that isn't URL-safe never reaches the network
    assert!(r.join_invite("../users/@me", "sess", ctx, None).await.is_err());
}

#[tokio::test]
async fn captcha_is_recognised() {
    use rxcord_api::rest::InviteContext;
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x"}"#, "application/json"))
        .mount(&s)
        .await;
    let r = Rest::with_base("tok".into(), s.uri());
    let ctx = InviteContext { guild: Some(Id(1)), channel: Id(5), channel_type: 0, message: Id(6) };
    let e = r.join_invite("abc", "sess", ctx, None).await.unwrap_err();
    assert!(e.is_captcha());
    assert!(!ApiError::Http { status: 400, body: "{}".into() }.is_captcha());
}

#[tokio::test]
async fn join_invite_sends_captcha_answer() {
    use rxcord_api::rest::{CaptchaAnswer, InviteContext};
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("X-Captcha-Key", "solved"))
        .and(header("X-Captcha-Rqtoken", "rq"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"guild":{"id":"1"}}"#, "application/json"))
        .expect(1)
        .mount(&s)
        .await;
    let r = Rest::with_base("tok".into(), s.uri());
    let ctx = InviteContext { guild: None, channel: Id(5), channel_type: 1, message: Id(6) };
    let a = CaptchaAnswer { key: "solved".into(), rqtoken: Some("rq".into()), session_id: None };
    assert!(r.join_invite("abc", "sess", ctx, Some(&a)).await.is_ok());
}

#[tokio::test]
async fn ring_bodies() {
    let s = MockServer::start().await;
    Mock::given(method("POST")).and(path("/channels/4/call/ring")).and(body_json(json!({"recipients": null})))
        .respond_with(ResponseTemplate::new(204)).expect(1).mount(&s).await;
    Mock::given(method("POST")).and(path("/channels/4/call/stop-ringing")).and(body_json(json!({"recipients": ["8"]})))
        .respond_with(ResponseTemplate::new(204)).expect(1).mount(&s).await;
    let r = Rest::with_base("tok".into(), s.uri());
    r.ring(Id(4)).await.unwrap();
    r.stop_ringing(Id(4), Id(8)).await.unwrap();
}
