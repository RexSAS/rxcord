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
        .and(body_json(json!({"content":"hi","nonce":"n1","message_reference":{"message_id":"9"}})))
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
