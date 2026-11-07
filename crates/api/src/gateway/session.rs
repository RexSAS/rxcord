//! Sans-IO gateway session: the driver feeds it text/timer/close events and executes the `Action`s.
use crate::client_props;
use crate::id::Id;
use crate::event::{Event, parse_dispatch};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use std::{fmt, time::Duration};

/// Commands the app sends over the main gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayCmd {
    VoiceState { guild_id: Option<Id>, channel_id: Option<Id>, self_mute: bool, self_deaf: bool },
}

pub enum Action {
    Send(String),
    Dispatch(Event),
    Reconnect { resume: bool },
    AuthFailed,
    /// Close code that reconnecting won't fix (bad shard/intents/version).
    Fatal(u16),
    /// READY arrived but couldn't be parsed: the account data is missing.
    ReadyFailed,
}

// Send carries Identify/Resume (with the token), so its payload is never printed.
impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Send(s) => write!(f, "Send(<{} bytes redacted>)", s.len()),
            Action::Dispatch(e) => f.debug_tuple("Dispatch").field(e).finish(),
            Action::Reconnect { resume } => f.debug_struct("Reconnect").field("resume", resume).finish(),
            Action::AuthFailed => f.write_str("AuthFailed"),
            Action::Fatal(c) => write!(f, "Fatal({c})"),
            Action::ReadyFailed => f.write_str("ReadyFailed"),
        }
    }
}

pub struct Session {
    token: String,
    session_id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
    interval: Option<Duration>,
    awaiting_ack: bool,
    /// Between READY/RESUMED and the next close or Hello: commands can go out.
    online: bool,
    last_voice: Option<GatewayCmd>,
    /// `last_voice` was set while offline and never sent.
    voice_unsent: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("token", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Envelope {
    op: u8,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    d: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct ReadyInfo {
    session_id: String,
    resume_gateway_url: String,
}

impl Session {
    pub fn new(token: String) -> Self {
        Self { token, session_id: None, resume_url: None, seq: None, interval: None, awaiting_ack: false, online: false, last_voice: None, voice_unsent: false }
    }

    pub fn resume_url(&self) -> Option<&str> {
        self.resume_url.as_deref()
    }

    pub fn heartbeat_interval(&self) -> Option<Duration> {
        self.interval
    }

    fn clear(&mut self) {
        self.session_id = None;
        self.resume_url = None;
        self.seq = None;
    }

    fn heartbeat(&mut self) -> Action {
        self.awaiting_ack = true;
        Action::Send(json!({"op": 1, "d": self.seq}).to_string())
    }

    pub fn on_heartbeat_due(&mut self) -> Vec<Action> {
        if self.awaiting_ack {
            return vec![Action::Reconnect { resume: true }];
        }
        vec![self.heartbeat()]
    }

    /// Op 4. Sent now if connected; otherwise remembered and replayed after the next READY/RESUMED.
    pub fn command(&mut self, cmd: GatewayCmd) -> Option<Action> {
        let GatewayCmd::VoiceState { .. } = cmd;
        self.last_voice = Some(cmd);
        self.voice_unsent = !self.online;
        self.online.then(|| self.voice_action()).flatten()
    }

    fn voice_action(&mut self) -> Option<Action> {
        self.voice_unsent = false;
        let GatewayCmd::VoiceState { guild_id, channel_id, self_mute, self_deaf } = self.last_voice?;
        let d = json!({
            "guild_id": guild_id.map(|g| g.to_string()),
            "channel_id": channel_id.map(|c| c.to_string()),
            "self_mute": self_mute,
            "self_deaf": self_deaf,
            "self_video": false,
        });
        Some(Action::Send(json!({"op": 4, "d": d}).to_string()))
    }

    pub fn on_close(&mut self, code: Option<u16>) -> Action {
        self.online = false;
        match code {
            Some(4004) => Action::AuthFailed,
            Some(c @ 4010..=4014) => Action::Fatal(c),
            Some(4007 | 4009) => {
                self.clear();
                Action::Reconnect { resume: false }
            }
            _ => Action::Reconnect { resume: true },
        }
    }

    fn handshake(&self) -> Value {
        match (&self.session_id, self.seq) {
            (Some(id), Some(seq)) => json!({"op": 6, "d": {"token": self.token, "session_id": id, "seq": seq}}),
            _ => json!({"op": 2, "d": {
                "token": self.token,
                "capabilities": client_props::CAPABILITIES,
                "properties": client_props::super_properties_json(),
                "presence": {"status": "unknown", "since": 0, "activities": [], "afk": false},
                "compress": false,
                "client_state": {"guild_versions": {}},
            }}),
        }
    }

    pub fn on_text(&mut self, text: &str) -> Vec<Action> {
        let env: Envelope = match serde_json::from_str(text) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("malformed gateway frame: {e}");
                return vec![];
            }
        };
        let d = env.d.as_deref().map(RawValue::get).unwrap_or("null");
        match env.op {
            0 => self.dispatch(env.t.as_deref().unwrap_or(""), env.s, env.d.as_deref()),
            1 => vec![self.heartbeat()],
            7 => {
                self.online = false;
                vec![Action::Reconnect { resume: true }]
            }
            9 => {
                self.online = false;
                let resume = d == "true";
                if !resume {
                    self.clear();
                }
                vec![Action::Reconnect { resume }]
            }
            10 => {
                #[derive(Deserialize)]
                struct Hello {
                    heartbeat_interval: u64,
                }
                match serde_json::from_str::<Hello>(d) {
                    Ok(h) => {
                        self.interval = Some(Duration::from_millis(h.heartbeat_interval));
                        self.awaiting_ack = false;
                        self.online = false;
                        vec![Action::Send(self.handshake().to_string())]
                    }
                    Err(e) => {
                        tracing::warn!("malformed hello: {e}");
                        vec![]
                    }
                }
            }
            11 => {
                self.awaiting_ack = false;
                vec![]
            }
            _ => vec![],
        }
    }

    fn dispatch(&mut self, name: &str, seq: Option<u64>, d: Option<&RawValue>) -> Vec<Action> {
        let Some(d) = d else {
            tracing::warn!("dispatch {name} without data");
            return vec![];
        };
        // seq must track every dispatch, even ones we cannot parse.
        if seq.is_some() {
            self.seq = seq;
        }
        if name == "READY" {
            match serde_json::from_str::<ReadyInfo>(d.get()) {
                Ok(r) => {
                    self.session_id = Some(r.session_id);
                    self.resume_url = Some(r.resume_gateway_url);
                }
                Err(e) => tracing::warn!("READY without session info: {e}"),
            }
        }
        let event = match parse_dispatch(name, d) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("malformed {name}: {e}");
                return if name == "READY" { vec![Action::ReadyFailed] } else { vec![] };
            }
        };
        let mut out = vec![];
        let replay = match &event {
            Event::Ready(_) => {
                self.online = true;
                matches!(self.last_voice, Some(GatewayCmd::VoiceState { channel_id: Some(_), .. }))
            }
            Event::Resumed => {
                self.online = true;
                self.voice_unsent
            }
            _ => false,
        };
        out.push(Action::Dispatch(event));
        if replay {
            out.extend(self.voice_action());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const TOKEN: &str = "SECRET-TOKEN-123";
    fn hello() -> String {
        json!({"op":10,"d":{"heartbeat_interval":41250}}).to_string()
    }
    fn sent(a: &[Action], i: usize) -> Value {
        match &a[i] {
            Action::Send(s) => serde_json::from_str(s).unwrap(),
            o => panic!("expected Send, got {o:?}"),
        }
    }
    fn ready_session() -> Session {
        let mut s = Session::new(TOKEN.into());
        s.on_text(&hello());
        let r = json!({"op":0,"t":"READY","s":5,"d":{"v":9,"user":{"id":"1","username":"u","discriminator":"0"},
            "session_id":"sess","resume_gateway_url":"wss://resume.example","guilds":[],"private_channels":[]}});
        let a = s.on_text(&r.to_string());
        assert!(matches!(a.as_slice(), [Action::Dispatch(_)]), "{a:?}");
        s
    }

    #[test]
    fn hello_without_session_identifies() {
        let mut s = Session::new(TOKEN.into());
        let a = s.on_text(&hello());
        let v = sent(&a, 0);
        assert_eq!(v["op"], 2);
        assert_eq!(v["d"]["token"], TOKEN);
        assert_eq!(v["d"]["capabilities"], crate::client_props::CAPABILITIES);
        assert_eq!(s.heartbeat_interval(), Some(Duration::from_millis(41250)));
    }

    #[test]
    fn hello_with_session_resumes() {
        let mut s = ready_session();
        assert_eq!(s.resume_url(), Some("wss://resume.example"));
        let a = s.on_text(&hello());
        let v = sent(&a, 0);
        assert_eq!(v["op"], 6);
        assert_eq!(v["d"]["seq"], 5);
        assert_eq!(v["d"]["session_id"], "sess");
    }

    #[test]
    fn zombie_reconnects() {
        let mut s = ready_session();
        assert_eq!(sent(&s.on_heartbeat_due(), 0), json!({"op":1,"d":5}));
        let a = s.on_heartbeat_due();
        assert!(matches!(a.as_slice(), [Action::Reconnect { resume: true }]));
    }

    #[test]
    fn ack_clears_zombie() {
        let mut s = ready_session();
        s.on_heartbeat_due();
        s.on_text(&json!({"op":11}).to_string());
        assert!(matches!(s.on_heartbeat_due().as_slice(), [Action::Send(_)]));
    }

    #[test]
    fn invalid_session_false_clears() {
        let mut s = ready_session();
        let a = s.on_text(&json!({"op":9,"d":false}).to_string());
        assert!(matches!(a.as_slice(), [Action::Reconnect { resume: false }]));
        assert_eq!(s.resume_url(), None);
        assert_eq!(sent(&s.on_text(&hello()), 0)["op"], 2);
    }

    #[test]
    fn invalid_session_true_and_op7_resume() {
        let mut s = ready_session();
        let a = s.on_text(&json!({"op":9,"d":true}).to_string());
        assert!(matches!(a.as_slice(), [Action::Reconnect { resume: true }]));
        let a = s.on_text(&json!({"op":7,"d":null}).to_string());
        assert!(matches!(a.as_slice(), [Action::Reconnect { resume: true }]));
    }

    #[test]
    fn op1_sends_heartbeat_and_malformed_ignored() {
        let mut s = ready_session();
        let a = s.on_text(&json!({"op":1,"d":null}).to_string());
        assert_eq!(sent(&a, 0), json!({"op":1,"d":5}));
        assert!(s.on_text("garbage").is_empty());
        assert!(s.on_text(&json!({"op":0,"t":"GUILD_CREATE","s":6,"d":"bad"}).to_string()).is_empty());
    }

    #[test]
    fn close_codes() {
        let mut s = ready_session();
        assert!(matches!(s.on_close(Some(4004)), Action::AuthFailed));
        assert!(matches!(s.on_close(None), Action::Reconnect { resume: true }));
        assert!(matches!(s.on_close(Some(4007)), Action::Reconnect { resume: false }));
        assert_eq!(s.resume_url(), None);
        for code in 4010..=4014 {
            assert!(matches!(s.on_close(Some(code)), Action::Fatal(c) if c == code));
        }
        assert!(matches!(s.on_close(Some(4015)), Action::Reconnect { resume: true }));
    }

    #[test]
    fn action_debug_redacts_token() {
        let mut s = Session::new(TOKEN.into());
        let a = s.on_text(&hello());
        assert!(!format!("{a:?}").contains(TOKEN));
    }

    #[test]
    fn malformed_dispatch_still_advances_seq() {
        let mut s = ready_session();
        let bad = json!({"op":0,"t":"MESSAGE_CREATE","s":9,"d":"bad"});
        assert!(s.on_text(&bad.to_string()).is_empty());
        let a = s.on_text(&hello());
        assert_eq!(sent(&a, 0)["d"]["seq"], 9);
    }

    #[test]
    fn ready_session_info_survives_bad_ready_body() {
        let mut s = Session::new(TOKEN.into());
        let r = json!({"op":0,"t":"READY","s":3,"d":{"user":"bad","session_id":"sess","resume_gateway_url":"wss://r"}});
        assert!(matches!(s.on_text(&r.to_string()).as_slice(), [Action::ReadyFailed]));
        assert_eq!(s.resume_url(), Some("wss://r"));
        let v = sent(&s.on_text(&hello()), 0);
        assert_eq!((v["op"].as_u64(), v["d"]["seq"].as_u64()), (Some(6), Some(3)));
    }

    fn vs(channel: Option<u64>) -> GatewayCmd {
        GatewayCmd::VoiceState { guild_id: None, channel_id: channel.map(Id), self_mute: true, self_deaf: false }
    }

    #[test]
    fn voice_state_json_shape() {
        let mut s = ready_session();
        let a = s.command(vs(Some(7))).unwrap();
        assert_eq!(
            sent(&[a], 0),
            json!({"op":4,"d":{"guild_id":null,"channel_id":"7","self_mute":true,"self_deaf":false,"self_video":false}})
        );
        let GatewayCmd::VoiceState { guild_id, .. } = vs(None);
        assert_eq!(guild_id, None);
        let a = s.command(GatewayCmd::VoiceState { guild_id: Some(Id(3)), channel_id: None, self_mute: false, self_deaf: false });
        let v = sent(&[a.unwrap()], 0);
        assert_eq!((v["d"]["guild_id"].as_str(), v["d"]["channel_id"].is_null()), (Some("3"), true));
    }

    #[test]
    fn voice_state_not_sent_offline_and_replayed_after_identify() {
        let mut s = Session::new(TOKEN.into());
        assert!(s.command(vs(Some(7))).is_none());
        s.on_text(&hello());
        let r = json!({"op":0,"t":"READY","s":1,"d":{"user":{"id":"1","username":"u"},"session_id":"x","resume_gateway_url":"wss://r"}});
        let a = s.on_text(&r.to_string());
        assert!(matches!(a[0], Action::Dispatch(_)));
        assert_eq!(sent(&a, 1)["d"]["channel_id"], "7");
        // Dropped connection: not online until the next READY; re-identify replays.
        s.on_close(None);
        assert!(s.command(vs(Some(8))).is_none());
        s.on_text(&json!({"op":9,"d":false}).to_string());
        s.on_text(&hello());
        let a = s.on_text(&r.to_string());
        assert_eq!(sent(&a, 1)["d"]["channel_id"], "8");
    }

    #[test]
    fn leave_is_not_replayed_on_identify_but_is_on_resume() {
        let mut s = ready_session();
        s.on_close(None);
        assert!(s.command(vs(None)).is_none());
        let a = s.on_text(&json!({"op":0,"t":"RESUMED","s":6,"d":{}}).to_string());
        assert_eq!(sent(&a, 1)["d"]["channel_id"], serde_json::Value::Null);
        // A later READY must not resend a leave.
        let r = json!({"op":0,"t":"READY","s":7,"d":{"user":{"id":"1"},"session_id":"x","resume_gateway_url":"wss://r"}});
        assert_eq!(s.on_text(&r.to_string()).len(), 1);
    }

    #[test]
    fn debug_redacts_token() {
        let s = Session::new(TOKEN.into());
        assert!(!format!("{s:?}").contains(TOKEN));
    }
}
