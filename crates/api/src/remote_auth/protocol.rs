//! Sans-IO remote-auth protocol state machine.
use base64::{Engine, engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey, pkcs8::EncodePublicKey};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum RaAction {
    Send(String),
    ShowQr(String),
    Scanned { username: String },
    Ticket(String),
    Cancelled,
    StartHeartbeat(Duration),
}

#[derive(Debug, thiserror::Error)]
pub enum RaError {
    #[error("bad frame: {0}")]
    Frame(String),
    #[error("crypto: {0}")]
    Crypto(String),
}

pub struct RaProtocol {
    key: RsaPrivateKey,
}

// Manual Debug: never print the private key.
impl std::fmt::Debug for RaProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RaProtocol { .. }")
    }
}

impl Default for RaProtocol {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Frame {
    op: String,
    heartbeat_interval: Option<u64>,
    encrypted_nonce: Option<String>,
    fingerprint: Option<String>,
    encrypted_user_payload: Option<String>,
    ticket: Option<String>,
}

fn send(v: serde_json::Value) -> RaAction {
    RaAction::Send(v.to_string())
}

impl RaProtocol {
    pub fn new() -> Self {
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("rsa keygen");
        Self { key }
    }

    fn decrypt(&self, b64: &str) -> Result<Vec<u8>, RaError> {
        let ct = STANDARD.decode(b64).map_err(|e| RaError::Crypto(e.to_string()))?;
        self.key.decrypt(Oaep::new::<Sha256>(), &ct).map_err(|e| RaError::Crypto(e.to_string()))
    }

    fn need<'a>(v: &'a Option<String>, name: &str) -> Result<&'a str, RaError> {
        v.as_deref().ok_or_else(|| RaError::Frame(format!("missing {name}")))
    }

    pub fn on_text(&mut self, text: &str) -> Result<Vec<RaAction>, RaError> {
        let f: Frame = serde_json::from_str(text).map_err(|e| RaError::Frame(e.to_string()))?;
        Ok(match f.op.as_str() {
            "hello" => {
                let der = RsaPublicKey::from(&self.key).to_public_key_der().map_err(|e| RaError::Crypto(e.to_string()))?;
                let ms = f.heartbeat_interval.ok_or_else(|| RaError::Frame("missing heartbeat_interval".into()))?;
                vec![
                    send(json!({"op": "init", "encoded_public_key": STANDARD.encode(der.as_bytes())})),
                    RaAction::StartHeartbeat(Duration::from_millis(ms)),
                ]
            }
            "nonce_proof" => {
                let nonce = self.decrypt(Self::need(&f.encrypted_nonce, "encrypted_nonce")?)?;
                vec![send(json!({"op": "nonce_proof", "proof": URL_SAFE_NO_PAD.encode(Sha256::digest(nonce))}))]
            }
            "pending_remote_init" => {
                vec![RaAction::ShowQr(format!("https://discord.com/ra/{}", Self::need(&f.fingerprint, "fingerprint")?))]
            }
            "pending_ticket" => {
                let payload = self.decrypt(Self::need(&f.encrypted_user_payload, "encrypted_user_payload")?)?;
                let payload = String::from_utf8_lossy(&payload);
                // "id:discriminator:avatar:username"
                let username = payload.splitn(4, ':').nth(3).unwrap_or_default().to_string();
                vec![RaAction::Scanned { username }]
            }
            "pending_login" => vec![RaAction::Ticket(Self::need(&f.ticket, "ticket")?.to_string())],
            "cancel" => vec![RaAction::Cancelled],
            _ => vec![],
        })
    }

    pub fn decrypt_token(&self, encrypted_b64: &str) -> Result<String, RaError> {
        String::from_utf8(self.decrypt(encrypted_b64)?).map_err(|_| RaError::Crypto("token not utf-8".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePublicKey;

    fn sent(a: &RaAction) -> serde_json::Value {
        match a {
            RaAction::Send(s) => serde_json::from_str(s).unwrap(),
            o => panic!("not Send: {o:?}"),
        }
    }
    fn pubkey(p: &mut RaProtocol) -> RsaPublicKey {
        let acts = p.on_text(r#"{"op":"hello","heartbeat_interval":41250,"timeout_ms":120000}"#).unwrap();
        let init = sent(&acts[0]);
        RsaPublicKey::from_public_key_der(&STANDARD.decode(init["encoded_public_key"].as_str().unwrap()).unwrap()).unwrap()
    }
    fn enc(k: &RsaPublicKey, data: &[u8]) -> String {
        STANDARD.encode(k.encrypt(&mut rsa::rand_core::OsRng, Oaep::new::<Sha256>(), data).unwrap())
    }

    #[test]
    fn hello_sends_init_with_pubkey() {
        let mut p = RaProtocol::new();
        let acts = p.on_text(r#"{"op":"hello","heartbeat_interval":41250,"timeout_ms":120000}"#).unwrap();
        assert_eq!(sent(&acts[0])["op"], "init");
        assert_eq!(acts[1], RaAction::StartHeartbeat(Duration::from_millis(41250)));
        let k = RsaPublicKey::from_public_key_der(
            &STANDARD.decode(sent(&acts[0])["encoded_public_key"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(rsa::traits::PublicKeyParts::size(&k) * 8, 2048);
    }

    #[test]
    fn nonce_proof_roundtrip() {
        let mut p = RaProtocol::new();
        let k = pubkey(&mut p);
        let frame = format!(r#"{{"op":"nonce_proof","encrypted_nonce":"{}"}}"#, enc(&k, b"abc"));
        let acts = p.on_text(&frame).unwrap();
        let m = sent(&acts[0]);
        assert_eq!(m["op"], "nonce_proof");
        assert_eq!(m["proof"], URL_SAFE_NO_PAD.encode(Sha256::digest(b"abc")));
    }

    #[test]
    fn fingerprint_to_qr_url() {
        let mut p = RaProtocol::new();
        let acts = p.on_text(r#"{"op":"pending_remote_init","fingerprint":"abc-DEF"}"#).unwrap();
        assert_eq!(acts, vec![RaAction::ShowQr("https://discord.com/ra/abc-DEF".into())]);
    }

    #[test]
    fn ticket_payload_username() {
        let mut p = RaProtocol::new();
        let k = pubkey(&mut p);
        let frame = format!(r#"{{"op":"pending_ticket","encrypted_user_payload":"{}"}}"#, enc(&k, b"1:0:x:alice"));
        assert_eq!(p.on_text(&frame).unwrap(), vec![RaAction::Scanned { username: "alice".into() }]);
    }

    #[test]
    fn login_cancel_and_token() {
        let mut p = RaProtocol::new();
        let k = pubkey(&mut p);
        assert_eq!(p.on_text(r#"{"op":"pending_login","ticket":"T"}"#).unwrap(), vec![RaAction::Ticket("T".into())]);
        assert_eq!(p.on_text(r#"{"op":"cancel"}"#).unwrap(), vec![RaAction::Cancelled]);
        assert_eq!(p.decrypt_token(&enc(&k, b"tok.en")).unwrap(), "tok.en");
        assert!(!format!("{p:?}").contains("PRIVATE"));
    }

    #[test]
    fn unknown_op_ignored() {
        let mut p = RaProtocol::new();
        assert_eq!(p.on_text(r#"{"op":"heartbeat_ack"}"#).unwrap(), vec![]);
    }
}
