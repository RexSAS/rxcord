//! Sans-IO voice gateway session: the driver feeds it frames/close events and executes the `Action`s.
use crate::rtp::Mode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fmt, time::Duration};

pub struct Connect {
    pub server_id: u64,
    pub user_id: u64,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

impl fmt::Debug for Connect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connect")
            .field("server_id", &self.server_id)
            .field("user_id", &self.user_id)
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Never printed: Identify/Resume carry the voice token, DAVE frames carry MLS material.
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Frame::Text(s) => write!(f, "Text(<{} bytes redacted>)", s.len()),
            Frame::Binary(b) => write!(f, "Binary(<{} bytes redacted>)", b.len()),
        }
    }
}

#[derive(Debug)]
pub enum DaveIn {
    PrepareTransition { id: u16, version: u16 },
    ExecuteTransition(u16),
    PrepareEpoch { epoch: u64, version: u16 },
    ExternalSender(Vec<u8>),
    Proposals { op: u8, data: Vec<u8> },
    AnnounceCommit { id: u16, commit: Vec<u8> },
    Welcome { id: u16, welcome: Vec<u8> },
}

pub enum DaveOut {
    KeyPackage(Vec<u8>),
    CommitWelcome(Vec<u8>),
    TransitionReady(u16),
    InvalidCommitWelcome(u16),
}

#[derive(Debug)]
pub enum LeaveReason {
    Kicked,
    BadEncryption,
    Fatal(u16),
    /// Close 4006 again right after a fresh Identify: the main gateway must rejoin (op 4).
    SessionInvalid,
}

pub enum Action {
    Send(Frame),
    HeartbeatEvery(Duration),
    /// Op 6: the server answered our last heartbeat.
    HeartbeatAck,
    Discover {
        ip: String,
        port: u16,
        ssrc: u32,
    },
    Ready {
        mode: Mode,
        key: [u8; 32],
        dave_version: u16,
        ssrc: u32,
    },
    Speaking {
        user_id: u64,
        ssrc: u32,
        on: bool,
    },
    UserLeft(u64),
    UsersJoined(Vec<u64>),
    Dave(DaveIn),
    Resumed,
    Reconnect {
        resume: bool,
    },
    Leave(LeaveReason),
}

// Ready carries the transport secret key, so it is never printed.
impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Send(x) => f.debug_tuple("Send").field(x).finish(),
            Action::HeartbeatEvery(d) => f.debug_tuple("HeartbeatEvery").field(d).finish(),
            Action::HeartbeatAck => f.write_str("HeartbeatAck"),
            Action::Discover { ip, port, ssrc } => f
                .debug_struct("Discover")
                .field("ip", ip)
                .field("port", port)
                .field("ssrc", ssrc)
                .finish(),
            Action::Ready {
                mode,
                dave_version,
                ssrc,
                ..
            } => f
                .debug_struct("Ready")
                .field("mode", mode)
                .field("key", &"<redacted>")
                .field("dave_version", dave_version)
                .field("ssrc", ssrc)
                .finish(),
            Action::Speaking { user_id, ssrc, on } => f
                .debug_struct("Speaking")
                .field("user_id", user_id)
                .field("ssrc", ssrc)
                .field("on", on)
                .finish(),
            Action::UserLeft(u) => f.debug_tuple("UserLeft").field(u).finish(),
            Action::UsersJoined(u) => f.debug_tuple("UsersJoined").field(u).finish(),
            Action::Dave(d) => f.debug_tuple("Dave").field(d).finish(),
            Action::Resumed => f.write_str("Resumed"),
            Action::Reconnect { resume } => {
                f.debug_struct("Reconnect").field("resume", resume).finish()
            }
            Action::Leave(r) => f.debug_tuple("Leave").field(r).finish(),
        }
    }
}

pub struct Session {
    c: Connect,
    max_dave: u16,
    seq: Option<u64>,
    ran: bool,
    /// A 4006 closed the last session and no Ready followed yet.
    invalid: bool,
    mode: Option<Mode>,
    ssrc: u32,
}

fn id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        S(String),
        U(u64),
    }
    match N::deserialize(d)? {
        N::U(n) => Ok(n),
        N::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: f64,
}
#[derive(Deserialize)]
struct ReadyD {
    ssrc: u32,
    ip: String,
    port: u16,
    modes: Vec<String>,
}
#[derive(Deserialize)]
struct Desc {
    mode: String,
    secret_key: [u8; 32],
    #[serde(default)]
    dave_protocol_version: u16,
}
#[derive(Deserialize)]
struct SpeakingD {
    #[serde(deserialize_with = "id")]
    user_id: u64,
    ssrc: u32,
    speaking: u32,
}
#[derive(Deserialize)]
struct Left {
    #[serde(deserialize_with = "id")]
    user_id: u64,
}
#[derive(Deserialize)]
struct Joined {
    #[serde(deserialize_with = "ids")]
    user_ids: Vec<u64>,
}
fn ids<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u64>, D::Error> {
    #[derive(Deserialize)]
    struct W(#[serde(deserialize_with = "id")] u64);
    Ok(Vec::<W>::deserialize(d)?.into_iter().map(|w| w.0).collect())
}
#[derive(Deserialize)]
struct Trans {
    transition_id: u16,
    #[serde(default)]
    protocol_version: u16,
}
#[derive(Deserialize)]
struct Epoch {
    epoch: u64,
    protocol_version: u16,
}

impl Session {
    pub fn new(c: Connect, max_dave: u16) -> Self {
        Self {
            c,
            max_dave,
            seq: None,
            ran: false,
            invalid: false,
            mode: None,
            ssrc: 0,
        }
    }

    fn text(v: Value) -> Action {
        Action::Send(Frame::Text(v.to_string()))
    }

    pub fn on_open(&mut self) -> Vec<Action> {
        let c = &self.c;
        let v = if self.ran {
            json!({"op": 7, "d": {"server_id": c.server_id.to_string(), "session_id": c.session_id,
                "token": c.token, "seq_ack": self.seq_ack()}})
        } else {
            json!({"op": 0, "d": {"server_id": c.server_id.to_string(), "user_id": c.user_id.to_string(),
                "session_id": c.session_id, "token": c.token, "max_dave_protocol_version": self.max_dave}})
        };
        vec![Self::text(v)]
    }

    fn seq_ack(&self) -> i64 {
        self.seq.map_or(-1, |s| s as i64)
    }

    pub fn on_text(&mut self, text: &str) -> Vec<Action> {
        #[derive(Deserialize)]
        struct Env {
            op: u8,
            #[serde(default)]
            seq: Option<u64>,
            #[serde(default)]
            d: Value,
        }
        let Ok(env) = serde_json::from_str::<Env>(text) else {
            tracing::debug!("malformed voice frame");
            return vec![];
        };
        if env.seq.is_some() {
            self.seq = env.seq;
        }
        let d = env.d;
        let parsed = |this: &mut Self| -> Option<Vec<Action>> {
            Some(match env.op {
                8 => {
                    let h: Hello = serde_json::from_value(d).ok()?;
                    vec![Action::HeartbeatEvery(Duration::from_millis(
                        h.heartbeat_interval as u64,
                    ))]
                }
                2 => {
                    let r: ReadyD = serde_json::from_value(d).ok()?;
                    let Some(mode) = Mode::choose(&r.modes) else {
                        return Some(vec![Action::Leave(LeaveReason::BadEncryption)]);
                    };
                    this.mode = Some(mode);
                    this.ssrc = r.ssrc;
                    vec![Action::Discover {
                        ip: r.ip,
                        port: r.port,
                        ssrc: r.ssrc,
                    }]
                }
                4 => {
                    let s: Desc = serde_json::from_value(d).ok()?;
                    let Some(mode) = Mode::from_name(&s.mode) else {
                        return Some(vec![Action::Leave(LeaveReason::BadEncryption)]);
                    };
                    (this.ran, this.invalid) = (true, false);
                    vec![Action::Ready {
                        mode,
                        key: s.secret_key,
                        dave_version: s.dave_protocol_version,
                        ssrc: this.ssrc,
                    }]
                }
                5 => {
                    let s: SpeakingD = serde_json::from_value(d).ok()?;
                    vec![Action::Speaking {
                        user_id: s.user_id,
                        ssrc: s.ssrc,
                        on: s.speaking != 0,
                    }]
                }
                6 => vec![Action::HeartbeatAck],
                9 => vec![Action::Resumed],
                11 => vec![Action::UsersJoined(
                    serde_json::from_value::<Joined>(d).ok()?.user_ids,
                )], // VERIFY LIVE
                13 => vec![Action::UserLeft(
                    serde_json::from_value::<Left>(d).ok()?.user_id,
                )], // VERIFY LIVE
                21 => {
                    let t: Trans = serde_json::from_value(d).ok()?;
                    vec![Action::Dave(DaveIn::PrepareTransition {
                        id: t.transition_id,
                        version: t.protocol_version,
                    })]
                }
                22 => {
                    let t: Trans = serde_json::from_value(d).ok()?;
                    vec![Action::Dave(DaveIn::ExecuteTransition(t.transition_id))]
                }
                24 => {
                    let e: Epoch = serde_json::from_value(d).ok()?;
                    vec![Action::Dave(DaveIn::PrepareEpoch {
                        epoch: e.epoch,
                        version: e.protocol_version,
                    })]
                }
                _ => vec![], // anything unknown
            })
        };
        parsed(self).unwrap_or_else(|| {
            tracing::debug!("malformed voice op {}", env.op);
            vec![]
        })
    }

    pub fn on_binary(&mut self, b: &[u8]) -> Vec<Action> {
        let [s0, s1, op, rest @ ..] = b else {
            return vec![];
        };
        self.seq = Some(u16::from_be_bytes([*s0, *s1]) as u64);
        let dave = match (*op, rest) {
            (25, r) => DaveIn::ExternalSender(r.to_vec()),
            (27, [o, r @ ..]) => DaveIn::Proposals {
                op: *o,
                data: r.to_vec(),
            },
            (29, [i0, i1, r @ ..]) => DaveIn::AnnounceCommit {
                id: u16::from_be_bytes([*i0, *i1]),
                commit: r.to_vec(),
            },
            (30, [i0, i1, r @ ..]) => DaveIn::Welcome {
                id: u16::from_be_bytes([*i0, *i1]),
                welcome: r.to_vec(),
            },
            _ => return vec![],
        };
        vec![Action::Dave(dave)]
    }

    pub fn on_discovered(&mut self, ip: &str, port: u16) -> Vec<Action> {
        let Some(mode) = self.mode else { return vec![] };
        vec![Self::text(json!({"op": 1, "d": {"protocol": "udp",
            "data": {"address": ip, "port": port, "mode": mode.name()}}}))]
    }

    pub fn heartbeat(&mut self, now_ms: u64) -> Action {
        Self::text(json!({"op": 3, "d": {"t": now_ms, "seq_ack": self.seq_ack()}}))
    }

    pub fn speaking(&self, on: bool) -> Action {
        Self::text(json!({"op": 5, "d": {"speaking": on as u8, "delay": 0, "ssrc": self.ssrc}}))
    }

    pub fn dave_send(&self, out: DaveOut) -> Action {
        let bin = |op: u8, mut d: Vec<u8>| {
            d.insert(0, op);
            Action::Send(Frame::Binary(d))
        };
        match out {
            DaveOut::KeyPackage(d) => bin(26, d),
            DaveOut::CommitWelcome(d) => bin(28, d),
            DaveOut::TransitionReady(id) => {
                Self::text(json!({"op": 23, "d": {"transition_id": id}}))
            }
            DaveOut::InvalidCommitWelcome(id) => {
                Self::text(json!({"op": 31, "d": {"transition_id": id}}))
            }
        }
    }

    pub fn on_close(&mut self, code: Option<u16>) -> Action {
        match code {
            Some(4014) => Action::Leave(LeaveReason::Kicked),
            Some(4016) => Action::Leave(LeaveReason::BadEncryption),
            Some(4006) if self.invalid => Action::Leave(LeaveReason::SessionInvalid),
            Some(4006) => {
                self.invalid = true;
                self.ran = false;
                self.seq = None;
                Action::Reconnect { resume: false }
            }
            Some(c @ 4017) => Action::Leave(LeaveReason::Fatal(c)),
            _ => Action::Reconnect { resume: true },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn conn() -> Connect {
        Connect {
            server_id: 1,
            user_id: 2,
            session_id: "sess".into(),
            token: "SECRET-TOKEN".into(),
            endpoint: "e.discord.media".into(),
        }
    }
    fn js(a: &Action) -> Value {
        match a {
            Action::Send(Frame::Text(t)) => serde_json::from_str(t).unwrap(),
            o => panic!("not text send: {o:?}"),
        }
    }
    fn ready(s: &mut Session) -> Vec<Action> {
        s.on_open();
        s.on_text(&json!({"op":8,"d":{"heartbeat_interval":41250.0}}).to_string());
        let r = s.on_text(&json!({"op":2,"d":{"ssrc":99,"ip":"1.2.3.4","port":5000,"modes":["xsalsa20","aead_xchacha20_poly1305_rtpsize","aead_aes256_gcm_rtpsize"]}}).to_string());
        assert!(
            matches!(&r[..], [Action::Discover { ip, port: 5000, ssrc: 99 }] if ip == "1.2.3.4")
        );
        s.on_discovered("9.9.9.9", 6000);
        s.on_text(&json!({"op":4,"seq":3,"d":{"mode":"aead_aes256_gcm_rtpsize","secret_key":vec![7u8;32],"dave_protocol_version":1}}).to_string())
    }

    #[test]
    fn handshake_order() {
        let mut s = Session::new(conn(), 1);
        let o = s.on_open();
        let v = js(&o[0]);
        assert_eq!(v["op"], 0);
        assert_eq!(v["d"]["max_dave_protocol_version"], 1);
        assert_eq!(v["d"]["server_id"], "1");
        assert_eq!(v["d"]["token"], "SECRET-TOKEN");
        let h = s.on_text(&json!({"op":8,"d":{"heartbeat_interval":41250.0}}).to_string());
        assert!(matches!(h[..], [Action::HeartbeatEvery(d)] if d == Duration::from_millis(41250)));
        s.on_text(&json!({"op":2,"d":{"ssrc":99,"ip":"1.2.3.4","port":5000,"modes":["aead_xchacha20_poly1305_rtpsize","aead_aes256_gcm_rtpsize"]}}).to_string());
        let d = s.on_discovered("9.9.9.9", 6000);
        let v = js(&d[0]);
        assert_eq!(v["op"], 1);
        assert_eq!(v["d"]["protocol"], "udp");
        assert_eq!(v["d"]["data"]["address"], "9.9.9.9");
        assert_eq!(v["d"]["data"]["port"], 6000);
        assert_eq!(v["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
        let r = s.on_text(&json!({"op":4,"d":{"mode":"aead_aes256_gcm_rtpsize","secret_key":vec![7u8;32],"dave_protocol_version":1}}).to_string());
        assert!(
            matches!(r[..], [Action::Ready { mode: Mode::Aes256Gcm, key, dave_version: 1, ssrc: 99 }] if key == [7; 32])
        );
    }

    #[test]
    fn no_supported_mode_leaves() {
        let mut s = Session::new(conn(), 1);
        s.on_open();
        let r = s.on_text(
            &json!({"op":2,"d":{"ssrc":1,"ip":"i","port":1,"modes":["xsalsa20"]}}).to_string(),
        );
        assert!(matches!(r[..], [Action::Leave(LeaveReason::BadEncryption)]));
    }

    #[test]
    fn heartbeat_carries_seq_ack() {
        let mut s = Session::new(conn(), 1);
        let v = js(&s.heartbeat(1234));
        assert_eq!(
            (
                v["op"].clone(),
                v["d"]["t"].clone(),
                v["d"]["seq_ack"].clone()
            ),
            (json!(3), json!(1234), json!(-1))
        );
        let ack = s.on_text(&json!({"op":6,"seq":7,"d":{"t":1234}}).to_string());
        assert!(matches!(ack[..], [Action::HeartbeatAck]), "the driver tracks ACKs");
        assert_eq!(js(&s.heartbeat(5))["d"]["seq_ack"], 7);
    }

    #[test]
    fn binary_dave_frames() {
        let mut s = Session::new(conn(), 1);
        assert!(
            matches!(&s.on_binary(&[0, 5, 25, 9, 9])[..], [Action::Dave(DaveIn::ExternalSender(d))] if d == &[9, 9])
        );
        assert!(
            matches!(&s.on_binary(&[0, 6, 27, 0, 1, 2])[..], [Action::Dave(DaveIn::Proposals { op: 0, data })] if data == &[1, 2])
        );
        assert!(
            matches!(&s.on_binary(&[0, 7, 29, 0, 3, 8])[..], [Action::Dave(DaveIn::AnnounceCommit { id: 3, commit })] if commit == &[8])
        );
        assert!(
            matches!(&s.on_binary(&[0, 8, 30, 0, 4, 6])[..], [Action::Dave(DaveIn::Welcome { id: 4, welcome })] if welcome == &[6])
        );
        assert_eq!(js(&s.heartbeat(0))["d"]["seq_ack"], 8);
        assert!(s.on_binary(&[0]).is_empty());
        assert!(s.on_binary(&[0, 9, 29, 0]).is_empty());
    }

    #[test]
    fn json_dave_frames() {
        let mut s = Session::new(conn(), 1);
        let mut t = |v: Value| s.on_text(&v.to_string());
        assert!(matches!(
            t(json!({"op":21,"d":{"transition_id":4,"protocol_version":1}}))[..],
            [Action::Dave(DaveIn::PrepareTransition {
                id: 4,
                version: 1
            })]
        ));
        assert!(matches!(
            t(json!({"op":22,"d":{"transition_id":4}}))[..],
            [Action::Dave(DaveIn::ExecuteTransition(4))]
        ));
        assert!(matches!(
            t(json!({"op":24,"d":{"epoch":1,"protocol_version":1}}))[..],
            [Action::Dave(DaveIn::PrepareEpoch {
                epoch: 1,
                version: 1
            })]
        ));
    }

    #[test]
    fn client_binary_layout() {
        let s = Session::new(conn(), 1);
        assert!(
            matches!(s.dave_send(DaveOut::KeyPackage(vec![1, 2])), Action::Send(Frame::Binary(b)) if b == [26, 1, 2])
        );
        assert!(
            matches!(s.dave_send(DaveOut::CommitWelcome(vec![3])), Action::Send(Frame::Binary(b)) if b == [28, 3])
        );
        let v = js(&s.dave_send(DaveOut::TransitionReady(9)));
        assert_eq!(
            (v["op"].clone(), v["d"]["transition_id"].clone()),
            (json!(23), json!(9))
        );
        let v = js(&s.dave_send(DaveOut::InvalidCommitWelcome(9)));
        assert_eq!(
            (v["op"].clone(), v["d"]["transition_id"].clone()),
            (json!(31), json!(9))
        );
    }

    #[test]
    fn speaking_events() {
        let mut s = Session::new(conn(), 1);
        let t = |s: &mut Session, v: Value| s.on_text(&v.to_string());
        assert!(matches!(
            t(
                &mut s,
                json!({"op":5,"d":{"user_id":"42","ssrc":7,"speaking":1}})
            )[..],
            [Action::Speaking {
                user_id: 42,
                ssrc: 7,
                on: true
            }]
        ));
        assert!(matches!(
            t(
                &mut s,
                json!({"op":5,"d":{"user_id":"42","ssrc":7,"speaking":0}})
            )[..],
            [Action::Speaking { on: false, .. }]
        ));
        assert!(matches!(
            t(&mut s, json!({"op":13,"d":{"user_id":"42"}}))[..],
            [Action::UserLeft(42)]
        ));
        assert!(
            matches!(&t(&mut s, json!({"op":11,"d":{"user_ids":["1","2"]}}))[..], [Action::UsersJoined(v)] if v == &[1, 2])
        );
        assert!(matches!(
            t(&mut s, json!({"op":9,"d":null}))[..],
            [Action::Resumed]
        ));
        assert!(t(&mut s, json!({"op":99,"d":1})).is_empty());
        assert!(s.on_text("garbage").is_empty());
        let mut s2 = Session::new(conn(), 1);
        ready(&mut s2);
        let v = js(&s2.speaking(true));
        assert_eq!(
            (
                v["op"].clone(),
                v["d"]["speaking"].clone(),
                v["d"]["ssrc"].clone()
            ),
            (json!(5), json!(1), json!(99))
        );
    }

    #[test]
    fn close_codes() {
        let mut s = Session::new(conn(), 1);
        assert!(matches!(
            s.on_close(Some(4014)),
            Action::Leave(LeaveReason::Kicked)
        ));
        assert!(matches!(
            s.on_close(Some(4016)),
            Action::Leave(LeaveReason::BadEncryption)
        ));
        assert!(matches!(
            s.on_close(Some(4015)),
            Action::Reconnect { resume: true }
        ));
        assert!(matches!(
            s.on_close(Some(4006)),
            Action::Reconnect { resume: false }
        ));
        assert!(matches!(
            s.on_close(Some(4017)),
            Action::Leave(LeaveReason::Fatal(4017))
        ));
        assert!(matches!(
            s.on_close(None),
            Action::Reconnect { resume: true }
        ));
    }

    #[test]
    fn resume_after_reconnect() {
        let mut s = Session::new(conn(), 1);
        ready(&mut s);
        let v = js(&s.on_open()[0]);
        assert_eq!(v["op"], 7);
        assert_eq!(v["d"]["seq_ack"], 3);
        assert_eq!(v["d"]["session_id"], "sess");
        s.on_close(Some(4006));
        assert_eq!(js(&s.on_open()[0])["op"], 0);
    }

    #[test]
    fn session_invalid_twice_leaves() {
        let mut s = Session::new(conn(), 1);
        ready(&mut s);
        assert!(matches!(s.on_close(Some(4006)), Action::Reconnect { resume: false }));
        s.on_open();
        assert!(
            matches!(s.on_close(Some(4006)), Action::Leave(LeaveReason::SessionInvalid)),
            "the fresh Identify failed too: the app rejoins through the main gateway"
        );
        let mut s = Session::new(conn(), 1);
        ready(&mut s);
        s.on_close(Some(4006));
        ready(&mut s);
        assert!(
            matches!(s.on_close(Some(4006)), Action::Reconnect { resume: false }),
            "a Ready in between: a new first failure"
        );
    }

    #[test]
    fn debug_redacts() {
        let mut s = Session::new(conn(), 1);
        let o = s.on_open();
        assert!(!format!("{o:?}").contains("SECRET-TOKEN"));
        assert!(!format!("{:?}", conn()).contains("SECRET-TOKEN"));
        let r = ready(&mut s);
        assert!(!format!("{r:?}").contains("7, 7"));
    }
}
