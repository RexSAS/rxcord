//! Async voice driver: voice WebSocket + UDP around the sans-IO `Session`, `Dave` and `Transport`.
//! Never logs tokens, keys or audio.
use crate::dave::Dave;
use crate::rtp::{RtpHeader, Transport, discovery_request, discovery_response};
use crate::session::{Action, Connect, Frame, LeaveReason, Session};
use futures_util::{SinkExt, StreamExt};
use rxcord_api::backoff::reconnect_delay;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const GIVE_UP: Duration = Duration::from_secs(30);
const PENDING_AGE: Duration = Duration::from_secs(1);
const PENDING_MAX: usize = 50;
/// This many unanswered heartbeats in a row: the link is dead.
const MISSED_ACKS: u32 = 2;
/// RTP timestamp step of one 20 ms frame at 48 kHz.
const FRAME_TS: u32 = 960;

pub enum VoiceEvent {
    Status(VoiceStatus),
    Speaking {
        user_id: u64,
        ssrc: u32,
        on: bool,
    },
    Media {
        user_id: u64,
        seq: u16,
        ts: u32,
        opus: Vec<u8>,
    },
    Left(LeaveReason),
    PrivacyCode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceStatus {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

pub enum VoiceCmd {
    /// One 20 ms Opus frame.
    Opus(Vec<u8>),
    Speaking(bool),
    Leave,
    /// A new voice server (VOICE_SERVER_UPDATE) or channel: start over there (fresh Identify, UDP and
    /// DAVE group) on these same channels, so the app's audio keeps flowing.
    Restart(Connect, u64),
}

/// Runs until `VoiceCmd::Leave`, either channel closing, a leave-worthy close/error (`Left`), or 30 s of
/// failed reconnects (`Left(Fatal(0))`). `channel_id` is the voice channel (DAVE group id).
pub async fn run_voice(
    c: Connect,
    channel_id: u64,
    events: mpsc::Sender<VoiceEvent>,
    cmds: mpsc::Receiver<VoiceCmd>,
) {
    run_voice_at(c, channel_id, events, cmds, true).await
}

fn voice_url(endpoint: &str) -> String {
    format!(
        "wss://{}/?v=8",
        endpoint.trim_start_matches("wss://").trim_end_matches('/')
    )
}

/// A server interval of 0/NaN must not make the loop spin.
fn heartbeat_period(d: Duration) -> Duration {
    d.max(Duration::from_secs(1))
}

/// `tls` false: plain `ws://{endpoint}` (tests' local fakes).
async fn run_voice_at(
    mut c: Connect,
    mut channel_id: u64,
    events: mpsc::Sender<VoiceEvent>,
    mut cmds: mpsc::Receiver<VoiceCmd>,
    tls: bool,
) {
    loop {
        let url = if tls { voice_url(&c.endpoint) } else { format!("ws://{}", c.endpoint) };
        match one_server(c, channel_id, events.clone(), &mut cmds, &url).await {
            Outcome::Restart(next, ch) => (c, channel_id) = (next, ch),
            _ => return,
        }
    }
}

/// One voice server, until it's left (`Done`) or the app moves us elsewhere (`Restart`).
async fn one_server(
    c: Connect,
    channel_id: u64,
    events: mpsc::Sender<VoiceEvent>,
    cmds: &mut mpsc::Receiver<VoiceCmd>,
    url: &str,
) -> Outcome {
    let me = c.user_id;
    let mut v = Voice {
        session: Session::new(c, davey::DAVE_PROTOCOL_VERSION),
        dave: Dave::new(me, channel_id),
        events,
        me,
        members: HashSet::from([me]),
        ssrcs: HashMap::new(),
        pending: Pending::default(),
        udp: None,
        transport: None,
        ssrc: 0,
        seq: rand::random(),
        ts: rand::random(),
        speaking: false,
        privacy: None,
        dropped: 0,
        healthy: false,
    };
    let mut attempt = 0u32;
    let mut down_since = Instant::now();
    loop {
        // connect, handshake and backoff share one budget: down for 30 s -> give up
        let deadline = down_since + GIVE_UP;
        let _ = v
            .events
            .send(VoiceEvent::Status(VoiceStatus::Connecting))
            .await;
        let ws = tokio::select! {
            r = timeout_at(deadline.min(Instant::now() + Duration::from_secs(10)), connect_async(url)) => match r {
                Ok(Ok((ws, _))) => Some(ws),
                Ok(Err(e)) => { tracing::warn!("voice connect failed: {e}"); None }
                Err(_) => { tracing::warn!("voice connect timed out"); None }
            },
            o = stopped(cmds, &v.events) => return o,
        };
        let outcome = match ws {
            Some(ws) => v.connection(ws, cmds, deadline).await,
            None => Outcome::Reconnect,
        };
        if !matches!(outcome, Outcome::Reconnect) {
            return outcome;
        }
        if std::mem::take(&mut v.healthy) {
            attempt = 0;
            down_since = Instant::now();
        }
        let deadline = down_since + GIVE_UP;
        if Instant::now() >= deadline {
            tracing::warn!("voice reconnect gave up");
            let _ = v.events.send(VoiceEvent::Left(LeaveReason::Fatal(0))).await;
            return Outcome::Done;
        }
        attempt += 1;
        let _ = v
            .events
            .send(VoiceEvent::Status(VoiceStatus::Reconnecting { attempt }))
            .await;
        tokio::select! {
            // the session picks Resume vs Identify on reopen, so the delay ignores the resume flag
            _ = sleep_until(deadline.min(Instant::now() + reconnect_delay(attempt - 1, true, rand::random(), rand::random()))) => {}
            o = stopped(cmds, &v.events) => return o,
        }
    }
}

/// Resolves when the app asks to leave or stops listening (`Done`), or moves us to another server
/// (`Restart`); other commands are dropped meanwhile.
async fn stopped(cmds: &mut mpsc::Receiver<VoiceCmd>, events: &mpsc::Sender<VoiceEvent>) -> Outcome {
    tokio::select! {
        _ = events.closed() => Outcome::Done,
        o = async {
            loop {
                match cmds.recv().await {
                    None | Some(VoiceCmd::Leave) => return Outcome::Done,
                    Some(VoiceCmd::Restart(c, ch)) => return Outcome::Restart(c, ch),
                    Some(_) => {}
                }
            }
        } => o,
    }
}

/// Binds a fresh UDP socket to the voice server and learns our external address (2 s timeout x 3).
async fn discover(ip: &str, port: u16, ssrc: u32) -> Option<(UdpSocket, String, u16)> {
    let udp = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    udp.connect((ip, port)).await.ok()?;
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        udp.send(&discovery_request(ssrc)).await.ok()?;
        if let Ok(Ok(n)) = timeout(Duration::from_secs(2), udp.recv(&mut buf)).await
            && let Some((ip, port)) = discovery_response(&buf[..n])
        {
            return Some((udp, ip, port));
        }
    }
    None
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum Outcome {
    Reconnect,
    Done,
    Restart(Connect, u64),
}

enum In {
    Stop,
    /// No Ready/Resumed before the reconnect budget ran out.
    Stalled,
    Cmd(VoiceCmd),
    Beat,
    Ws(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Udp(usize),
}

async fn send(ws: &mut Ws, a: Action) -> bool {
    let Action::Send(f) = a else { return true };
    let m = match f {
        Frame::Text(s) => Message::text(s),
        Frame::Binary(b) => Message::binary(b),
    };
    ws.send(m).await.is_ok()
}

/// Everything that outlives one WebSocket: a resume keeps UDP, keys, SSRCs and the DAVE group.
struct Voice {
    session: Session,
    dave: Dave,
    events: mpsc::Sender<VoiceEvent>,
    me: u64,
    /// Users in the call, always including us; fed to `Dave::set_members`.
    members: HashSet<u64>,
    ssrcs: HashMap<u32, u64>,
    pending: Pending,
    udp: Option<UdpSocket>,
    transport: Option<Transport>,
    ssrc: u32,
    seq: u16,
    ts: u32,
    speaking: bool,
    privacy: Option<String>,
    dropped: u64,
    /// Set once this connection reached Connected; resets the reconnect budget.
    healthy: bool,
}

impl Voice {
    /// Until Ready/Resumed the socket must get there by `deadline`, or it counts as a failed attempt.
    async fn connection(
        &mut self,
        mut ws: Ws,
        cmds: &mut mpsc::Receiver<VoiceCmd>,
        deadline: Instant,
    ) -> Outcome {
        let mut queue: VecDeque<Action> = self.session.on_open().into();
        let mut next_hb: Option<Instant> = None;
        let mut period = Duration::from_secs(40);
        let mut buf = vec![0u8; 2048];
        // heartbeats sent without an op 6 since; a link that silently died is noticed by this
        let mut unacked = 0u32;
        loop {
            while let Some(a) = queue.pop_front() {
                match a {
                    a @ Action::Send(_) => {
                        if !send(&mut ws, a).await {
                            return Outcome::Reconnect;
                        }
                    }
                    Action::HeartbeatEvery(d) => {
                        period = heartbeat_period(d);
                        next_hb = Some(Instant::now() + period);
                    }
                    Action::Discover { ip, port, ssrc } => {
                        let found = tokio::select! {
                            r = timeout_at(deadline, discover(&ip, port, ssrc)) => r.ok().flatten(),
                            o = stopped(cmds, &self.events) => {
                                let _ = ws.close(None).await;
                                return o;
                            }
                        };
                        let Some((udp, my_ip, my_port)) = found else {
                            tracing::warn!("voice ip discovery failed");
                            let _ = ws.close(None).await;
                            return Outcome::Reconnect;
                        };
                        self.udp = Some(udp);
                        queue.extend(self.session.on_discovered(&my_ip, my_port));
                    }
                    Action::Ready {
                        mode,
                        key,
                        dave_version,
                        ssrc,
                    } => {
                        self.transport = Some(Transport::new(mode, key, 0));
                        self.ssrc = ssrc;
                        self.speaking = false;
                        let outs = self.dave.start(dave_version);
                        queue.extend(outs.into_iter().map(|o| self.session.dave_send(o)));
                        self.privacy_changed().await;
                        self.connected().await;
                    }
                    Action::Resumed => self.connected().await,
                    Action::HeartbeatAck => unacked = 0,
                    Action::Speaking { user_id, ssrc, on } => {
                        self.ssrcs.insert(ssrc, user_id);
                        let _ = self
                            .events
                            .send(VoiceEvent::Speaking { user_id, ssrc, on })
                            .await;
                        for (h, p) in self.pending.take(Instant::now(), ssrc) {
                            self.deliver(user_id, h, &p);
                        }
                    }
                    Action::UsersJoined(ids) => {
                        self.members.extend(ids);
                        self.dave
                            .set_members(self.members.iter().copied().collect());
                    }
                    Action::UserLeft(id) => {
                        if id != self.me {
                            self.members.remove(&id);
                        }
                        self.ssrcs.retain(|_, u| *u != id);
                        self.dave
                            .set_members(self.members.iter().copied().collect());
                    }
                    Action::Dave(ev) => {
                        let outs = self.dave.handle(ev);
                        queue.extend(outs.into_iter().map(|o| self.session.dave_send(o)));
                        self.privacy_changed().await;
                    }
                    Action::Reconnect { .. } => {
                        let _ = ws.close(None).await;
                        return Outcome::Reconnect;
                    }
                    Action::Leave(r) => {
                        let _ = ws.close(None).await;
                        let _ = self.events.send(VoiceEvent::Left(r)).await;
                        return Outcome::Done;
                    }
                }
            }
            let input = tokio::select! {
                _ = self.events.closed() => In::Stop,
                _ = sleep_until(deadline), if !self.healthy => In::Stalled,
                c = cmds.recv() => c.map_or(In::Stop, In::Cmd),
                _ = async { sleep_until(next_hb.unwrap()).await }, if next_hb.is_some() => In::Beat,
                m = ws.next() => In::Ws(m),
                r = async { self.udp.as_ref().unwrap().recv(&mut buf).await }, if self.udp.is_some() => match r {
                    Ok(n) => In::Udp(n),
                    Err(e) => {
                        tracing::debug!("voice udp recv: {e}");
                        continue;
                    }
                },
            };
            match input {
                In::Stalled => {
                    tracing::warn!("voice handshake stalled");
                    let _ = ws.close(None).await;
                    return Outcome::Reconnect;
                }
                In::Stop | In::Cmd(VoiceCmd::Leave) => {
                    let _ = ws.close(None).await;
                    return Outcome::Done;
                }
                In::Cmd(VoiceCmd::Restart(c, ch)) => {
                    let _ = ws.close(None).await;
                    return Outcome::Restart(c, ch);
                }
                In::Cmd(VoiceCmd::Opus(f)) => {
                    if self.transport.is_none() {
                        continue; // not connected yet: drop
                    }
                    // op 5 speaking must precede the first frame after silence
                    if !self.speaking {
                        self.speaking = true;
                        if !send(&mut ws, self.session.speaking(true)).await {
                            return Outcome::Reconnect;
                        }
                    }
                    self.send_opus(&f);
                }
                In::Cmd(VoiceCmd::Speaking(on)) => {
                    if self.transport.is_none() {
                        continue; // no ssrc yet: Ready resets `speaking` and the first frame sends op 5
                    }
                    self.speaking = on;
                    queue.push_back(self.session.speaking(on));
                }
                In::Beat => {
                    if unacked >= MISSED_ACKS {
                        tracing::warn!("voice heartbeats not acknowledged: reconnecting");
                        let _ = ws.close(None).await;
                        return Outcome::Reconnect; // the session resumes if it got that far
                    }
                    unacked += 1;
                    next_hb = Some(Instant::now() + period);
                    let ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    queue.push_back(self.session.heartbeat(ms));
                }
                In::Ws(m) => queue.push_back(match m {
                    Some(Ok(Message::Text(t))) => {
                        queue.extend(self.session.on_text(&t));
                        continue;
                    }
                    Some(Ok(Message::Binary(b))) => {
                        queue.extend(self.session.on_binary(&b));
                        continue;
                    }
                    Some(Ok(Message::Close(f))) => {
                        self.session.on_close(f.map(|f| u16::from(f.code)))
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => self.session.on_close(None),
                }),
                In::Udp(n) => self.on_packet(&buf[..n]),
            }
        }
    }

    async fn connected(&mut self) {
        self.healthy = true;
        let _ = self
            .events
            .send(VoiceEvent::Status(VoiceStatus::Connected))
            .await;
    }

    async fn privacy_changed(&mut self) {
        let code = self.dave.privacy_code();
        if code != self.privacy {
            self.privacy = code.clone();
            if let Some(c) = code {
                let _ = self.events.send(VoiceEvent::PrivacyCode(c)).await;
            }
        }
    }

    /// DAVE then transport encryption; fails closed (no DAVE frame → nothing sent).
    fn send_opus(&mut self, opus: &[u8]) {
        let (Some(udp), Some(t)) = (&self.udp, &mut self.transport) else {
            return;
        };
        let Some(frame) = self.dave.encrypt(opus) else {
            return;
        };
        let h = RtpHeader {
            seq: self.seq,
            timestamp: self.ts,
            ssrc: self.ssrc,
        };
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(FRAME_TS);
        if let Err(e) = udp.try_send(&t.encrypt(&h, &frame)) {
            tracing::debug!("voice udp send: {e}");
        }
    }

    fn on_packet(&mut self, pkt: &[u8]) {
        let Some(t) = &self.transport else { return };
        let Some((h, mut payload)) = t.decrypt(pkt) else {
            return;
        }; // RTCP, stray or forged packets
        if pkt[0] & 0x20 != 0 {
            // P bit: the last byte counts the padding bytes (itself included)
            let pad = payload.last().copied().unwrap_or(0) as usize;
            if pad == 0 || pad > payload.len() {
                return;
            }
            payload.truncate(payload.len() - pad);
        }
        match self.ssrcs.get(&h.ssrc) {
            Some(&user_id) => self.deliver(user_id, h, &payload),
            None => self.pending.push(Instant::now(), h, payload),
        }
    }

    fn deliver(&mut self, user_id: u64, h: RtpHeader, payload: &[u8]) {
        let Some(opus) = self.dave.decrypt(user_id, payload) else {
            self.dropped += 1;
            tracing::debug!(
                user_id,
                dropped = self.dropped,
                "undecryptable voice frame dropped"
            );
            return;
        };
        // real-time: a full channel drops the frame rather than stalling the socket loop
        let _ = self.events.try_send(VoiceEvent::Media {
            user_id,
            seq: h.seq,
            ts: h.timestamp,
            opus,
        });
    }
}

/// Packets from SSRCs whose user isn't known yet (no Speaking): kept <= 1 s and <= 50, then dropped.
#[derive(Default)]
struct Pending(VecDeque<(Instant, RtpHeader, Vec<u8>)>);

impl Pending {
    fn prune(&mut self, now: Instant) {
        while self
            .0
            .front()
            .is_some_and(|p| now.duration_since(p.0) > PENDING_AGE)
        {
            self.0.pop_front();
        }
    }

    fn push(&mut self, now: Instant, h: RtpHeader, payload: Vec<u8>) {
        self.prune(now);
        if self.0.len() == PENDING_MAX {
            self.0.pop_front();
        }
        self.0.push_back((now, h, payload));
    }

    fn take(&mut self, now: Instant, ssrc: u32) -> Vec<(RtpHeader, Vec<u8>)> {
        self.prune(now);
        let (hit, keep) = std::mem::take(&mut self.0)
            .into_iter()
            .partition(|p| p.1.ssrc == ssrc);
        self.0 = keep;
        hit.into_iter().map(|(_, h, p)| (h, p)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{Mode, Transport};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use tokio::net::{TcpListener, TcpStream, UdpSocket};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
    use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

    const KEY: [u8; 32] = [7; 32];
    const T: Duration = Duration::from_secs(5);

    /// `endpoint`: host:port of a local fake (plain ws in tests).
    fn conn(endpoint: &str) -> Connect {
        Connect {
            server_id: 1,
            user_id: 2,
            session_id: "sess".into(),
            token: "tok".into(),
            endpoint: endpoint.into(),
        }
    }

    struct Fake {
        listener: TcpListener,
        ws: WebSocketStream<TcpStream>,
        udp: UdpSocket,
        client: SocketAddr,
        events: mpsc::Receiver<VoiceEvent>,
        cmds: mpsc::Sender<VoiceCmd>,
        task: JoinHandle<()>,
    }

    /// Starts the driver against a local fake and accepts its socket (handshake not done yet).
    async fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (etx, events) = mpsc::channel(64);
        let (cmds, crx) = mpsc::channel(64);
        let task = tokio::spawn(run_voice_at(conn(&addr), 3, etx, crx, false));
        let ws = accept(&listener).await;
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = udp.local_addr().unwrap(); // replaced by discovery
        Fake {
            listener,
            ws,
            udp,
            client,
            events,
            cmds,
            task,
        }
    }

    async fn accept(l: &TcpListener) -> WebSocketStream<TcpStream> {
        let (s, _) = timeout(T, l.accept()).await.unwrap().unwrap();
        accept_async(s).await.unwrap()
    }

    async fn recv_json(ws: &mut WebSocketStream<TcpStream>) -> Value {
        loop {
            if let Message::Text(t) = timeout(T, ws.next()).await.unwrap().unwrap().unwrap() {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    async fn send_json(ws: &mut WebSocketStream<TcpStream>, v: Value) {
        ws.send(Message::text(v.to_string())).await.unwrap();
    }

    async fn wait_event(
        rx: &mut mpsc::Receiver<VoiceEvent>,
        mut f: impl FnMut(&VoiceEvent) -> bool,
    ) -> VoiceEvent {
        loop {
            let ev = timeout(T, rx.recv()).await.unwrap().unwrap();
            if f(&ev) {
                return ev;
            }
        }
    }

    /// Identify → Hello → Ready → IP discovery → Select Protocol → Session Description → Connected.
    async fn handshake(f: &mut Fake) {
        handshake_dave(f, 0).await;
    }

    async fn handshake_dave(f: &mut Fake, dave: u16) {
        let id = recv_json(&mut f.ws).await;
        assert_eq!(id["op"], 0);
        assert_eq!(
            id["d"]["max_dave_protocol_version"],
            davey::DAVE_PROTOCOL_VERSION
        );
        after_identify(f, dave).await;
    }

    /// The handshake from Hello on.
    async fn after_identify(f: &mut Fake, dave: u16) {
        send_json(
            &mut f.ws,
            json!({"op": 8, "d": {"heartbeat_interval": 30000.0}}),
        )
        .await;
        let port = f.udp.local_addr().unwrap().port();
        send_json(
            &mut f.ws,
            json!({"op": 2, "d": {"ssrc": 1, "ip": "127.0.0.1", "port": port,
            "modes": ["xsalsa20_poly1305", "aead_aes256_gcm_rtpsize"]}}),
        )
        .await;
        let mut buf = [0u8; 128];
        let (n, from) = timeout(T, f.udp.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((n, &buf[..4]), (74, &[0, 1, 0, 70][..]));
        let mut resp = [0u8; 74];
        resp[..4].copy_from_slice(&[0, 2, 0, 70]);
        resp[8..17].copy_from_slice(b"127.0.0.1");
        resp[72..].copy_from_slice(&from.port().to_be_bytes());
        f.udp.send_to(&resp, from).await.unwrap();
        f.client = from;
        let sel = recv_json(&mut f.ws).await;
        assert_eq!(sel["op"], 1);
        assert_eq!(sel["d"]["data"]["port"], from.port());
        assert_eq!(sel["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
        send_json(
            &mut f.ws,
            json!({"op": 4, "d": {"mode": "aead_aes256_gcm_rtpsize", "secret_key": KEY,
            "dave_protocol_version": dave}}),
        )
        .await;
        wait_event(&mut f.events, |e| {
            matches!(e, VoiceEvent::Status(VoiceStatus::Connected))
        })
        .await;
    }

    fn packet(ssrc: u32, seq: u16, payload: &[u8]) -> Vec<u8> {
        let h = RtpHeader {
            seq,
            timestamp: 0,
            ssrc,
        };
        Transport::new(Mode::Aes256Gcm, KEY, seq as u32).encrypt(&h, payload)
    }

    /// RTP packet with the P bit set and `pad` padding bytes inside the ciphertext; the last says `claimed`.
    fn padded(ssrc: u32, seq: u16, payload: &[u8], pad: u8, claimed: u8) -> Vec<u8> {
        let mut pkt = vec![0xA0, 0x78];
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        let mut msg = payload.to_vec();
        msg.extend(vec![0; pad as usize - 1]);
        msg.push(claimed);
        let mut nonce = [0u8; 12];
        nonce[3] = 9;
        let ct = Aes256Gcm::new(&KEY.into())
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &msg,
                    aad: &pkt,
                },
            )
            .unwrap();
        pkt.extend_from_slice(&ct);
        pkt.extend_from_slice(&9u32.to_be_bytes());
        pkt
    }

    async fn media(rx: &mut mpsc::Receiver<VoiceEvent>) -> (u64, u16, Vec<u8>) {
        match wait_event(rx, |e| matches!(e, VoiceEvent::Media { .. })).await {
            VoiceEvent::Media {
                user_id, seq, opus, ..
            } => (user_id, seq, opus),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn connects_and_discovers() {
        let mut f = start().await;
        handshake(&mut f).await;
        f.cmds.send(VoiceCmd::Leave).await.unwrap();
        timeout(T, f.task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sends_encrypted_frames() {
        let mut f = start().await;
        handshake(&mut f).await;
        f.cmds.send(VoiceCmd::Opus(vec![1, 2, 3])).await.unwrap();
        f.cmds.send(VoiceCmd::Opus(vec![4, 5])).await.unwrap();
        let sp = recv_json(&mut f.ws).await;
        assert_eq!(
            (
                sp["op"].clone(),
                sp["d"]["speaking"].clone(),
                sp["d"]["ssrc"].clone()
            ),
            (json!(5), json!(1), json!(1))
        );
        let rx = Transport::new(Mode::Aes256Gcm, KEY, 0);
        let mut buf = [0u8; 256];
        let mut got = vec![];
        for _ in 0..2 {
            let n = timeout(T, f.udp.recv(&mut buf)).await.unwrap().unwrap();
            got.push(
                rx.decrypt(&buf[..n])
                    .expect("decrypts with the session key"),
            );
        }
        assert_eq!(got[0].0.ssrc, 1);
        assert_eq!(got[0].1, vec![1, 2, 3]);
        assert_eq!(got[1].1, vec![4, 5]);
        assert_eq!(got[1].0.seq, got[0].0.seq.wrapping_add(1));
        assert_eq!(got[1].0.timestamp, got[0].0.timestamp.wrapping_add(960));
        f.cmds.send(VoiceCmd::Speaking(false)).await.unwrap();
        let sp = recv_json(&mut f.ws).await;
        assert_eq!(
            (sp["op"].clone(), sp["d"]["speaking"].clone()),
            (json!(5), json!(0))
        );
    }

    #[tokio::test]
    async fn speaking_before_ready_is_dropped() {
        let mut f = start().await;
        assert_eq!(recv_json(&mut f.ws).await["op"], 0);
        f.cmds.send(VoiceCmd::Speaking(true)).await.unwrap(); // no ssrc yet
        tokio::time::sleep(Duration::from_millis(100)).await; // the idle driver takes it now
        after_identify(&mut f, 0).await; // asserts Select Protocol is the next text after Identify
        f.cmds.send(VoiceCmd::Opus(vec![1])).await.unwrap();
        let sp = recv_json(&mut f.ws).await;
        assert_eq!((sp["op"].clone(), sp["d"]["ssrc"].clone()), (json!(5), json!(1)), "op 5 only once connected, with our ssrc");
    }

    #[tokio::test]
    async fn dave_key_package_after_session_description() {
        let mut f = start().await;
        handshake_dave(&mut f, 1).await;
        let kp = loop {
            if let Message::Binary(b) = timeout(T, f.ws.next()).await.unwrap().unwrap().unwrap() {
                break b;
            }
        };
        assert!(kp.len() > 1 && kp[0] == 26, "binary op 26 with our key package");
        f.cmds.send(VoiceCmd::Opus(vec![1, 2, 3])).await.unwrap();
        let mut buf = [0u8; 256];
        assert!(
            timeout(Duration::from_millis(300), f.udp.recv(&mut buf)).await.is_err(),
            "no media before the DAVE group is ready"
        );
    }

    #[tokio::test]
    async fn media_from_known_ssrc() {
        let mut f = start().await;
        handshake(&mut f).await;
        send_json(
            &mut f.ws,
            json!({"op": 5, "d": {"user_id": "9", "ssrc": 77, "speaking": 1}}),
        )
        .await;
        let sp = wait_event(&mut f.events, |e| matches!(e, VoiceEvent::Speaking { .. })).await;
        assert!(matches!(
            sp,
            VoiceEvent::Speaking {
                user_id: 9,
                ssrc: 77,
                on: true
            }
        ));
        f.udp
            .send_to(&packet(77, 10, &[9, 9]), f.client)
            .await
            .unwrap();
        assert_eq!(media(&mut f.events).await, (9, 10, vec![9, 9]));
        // P bit: padding stripped; bogus padding length dropped
        f.udp
            .send_to(&padded(77, 11, &[8, 8, 8], 4, 4), f.client)
            .await
            .unwrap();
        assert_eq!(media(&mut f.events).await, (9, 11, vec![8, 8, 8]));
        f.udp
            .send_to(&padded(77, 12, &[1], 2, 200), f.client)
            .await
            .unwrap(); // claims more than it has
        f.udp
            .send_to(&padded(77, 12, &[1], 1, 0), f.client)
            .await
            .unwrap(); // zero padding length is invalid
        f.udp
            .send_to(&packet(77, 13, &[5]), f.client)
            .await
            .unwrap();
        assert_eq!(media(&mut f.events).await, (9, 13, vec![5]));
    }

    #[tokio::test]
    async fn unknown_ssrc_buffered_then_dropped() {
        let mut f = start().await;
        handshake(&mut f).await;
        f.udp.send_to(&packet(80, 5, &[5]), f.client).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await; // seq 5 is now older than 1 s
        f.udp.send_to(&packet(80, 6, &[6]), f.client).await.unwrap();
        send_json(
            &mut f.ws,
            json!({"op": 5, "d": {"user_id": 10, "ssrc": 80, "speaking": 1}}),
        )
        .await;
        assert_eq!(
            media(&mut f.events).await,
            (10, 6, vec![6]),
            "stale packet dropped, fresh one delivered"
        );
    }

    #[test]
    fn pending_keeps_one_second_and_fifty_packets() {
        let t0 = Instant::now();
        let h = |ssrc, seq| RtpHeader {
            seq,
            timestamp: 0,
            ssrc,
        };
        let mut p = Pending::default();
        p.push(t0, h(1, 1), vec![1]);
        p.push(t0, h(2, 1), vec![2]);
        p.push(t0 + Duration::from_millis(900), h(1, 2), vec![1]);
        let got: Vec<u16> = p
            .take(t0 + Duration::from_millis(1100), 1)
            .iter()
            .map(|x| x.0.seq)
            .collect();
        assert_eq!(got, vec![2], "older than 1 s dropped");
        assert!(p.take(t0 + Duration::from_millis(1100), 2).is_empty());
        for i in 0..60 {
            p.push(t0, h(3, i), vec![]);
        }
        let got = p.take(t0, 3);
        assert_eq!(
            (got.len(), got[0].0.seq),
            (50, 10),
            "capped at 50, oldest dropped"
        );
    }

    #[tokio::test]
    async fn leave_mid_handshake() {
        let mut f = start().await;
        assert_eq!(recv_json(&mut f.ws).await["op"], 0);
        send_json(
            &mut f.ws,
            json!({"op": 8, "d": {"heartbeat_interval": 30000.0}}),
        )
        .await;
        f.cmds.send(VoiceCmd::Leave).await.unwrap();
        timeout(T, f.task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn leave_during_reconnect_backoff() {
        let (etx, mut events) = mpsc::channel(64);
        let (cmds, crx) = mpsc::channel(8);
        let task = tokio::spawn(run_voice_at(conn("127.0.0.1:1"), 3, etx, crx, false));
        wait_event(&mut events, |e| {
            matches!(e, VoiceEvent::Status(VoiceStatus::Reconnecting { .. }))
        })
        .await;
        cmds.send(VoiceCmd::Leave).await.unwrap(); // backoff is >= 0.5 s; must end promptly
        timeout(Duration::from_millis(300), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_30s_of_failed_reconnects() {
        let (etx, mut events) = mpsc::channel(64);
        let (_cmds, crx) = mpsc::channel(8);
        let t0 = Instant::now();
        let task = tokio::spawn(run_voice_at(conn("127.0.0.1:1"), 3, etx, crx, false));
        let ev = timeout(Duration::from_secs(300), async {
            loop {
                if let ev @ VoiceEvent::Left(_) = events.recv().await.unwrap() {
                    return ev;
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(ev, VoiceEvent::Left(LeaveReason::Fatal(0))));
        assert!(t0.elapsed() >= Duration::from_secs(30));
        assert!(
            t0.elapsed() <= Duration::from_secs(31),
            "backoff must not overshoot the budget"
        );
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_when_handshake_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (etx, mut events) = mpsc::channel(64);
        let (_cmds, crx) = mpsc::channel(8);
        let t0 = Instant::now();
        let task = tokio::spawn(run_voice_at(conn(&addr), 3, etx, crx, false));
        // accepts and reads Identify, then never answers (sockets kept open)
        let (idents_tx, mut idents) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut open = vec![];
            while let Ok((s, _)) = listener.accept().await {
                let mut ws = accept_async(s).await.unwrap();
                if recv_json(&mut ws).await["op"] == 0 {
                    let _ = idents_tx.send(());
                }
                open.push(ws);
            }
        });
        let ev = timeout(Duration::from_secs(300), async {
            loop {
                if let ev @ VoiceEvent::Left(_) = events.recv().await.unwrap() {
                    return ev;
                }
            }
        })
        .await
        .expect("stalled handshake must give up");
        assert!(matches!(ev, VoiceEvent::Left(LeaveReason::Fatal(0))));
        assert!(t0.elapsed() <= Duration::from_secs(31));
        assert!(
            idents.try_recv().is_ok(),
            "a socket was opened and identified, then stalled"
        );
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn resumes_after_server_crash_close() {
        let mut f = start().await;
        handshake(&mut f).await;
        f.ws.close(Some(CloseFrame {
            code: CloseCode::from(4015),
            reason: "".into(),
        }))
        .await
        .unwrap();
        let mut ws = accept(&f.listener).await;
        let r = recv_json(&mut ws).await;
        assert_eq!(
            r["op"], 7,
            "session decides Resume after a completed handshake"
        );
        send_json(
            &mut ws,
            json!({"op": 8, "d": {"heartbeat_interval": 30000.0}}),
        )
        .await;
        send_json(&mut ws, json!({"op": 9, "d": null})).await;
        wait_event(&mut f.events, |e| {
            matches!(e, VoiceEvent::Status(VoiceStatus::Connected))
        })
        .await;
        // transport survives the resume
        f.cmds.send(VoiceCmd::Opus(vec![1])).await.unwrap();
        let mut buf = [0u8; 256];
        let n = timeout(T, f.udp.recv(&mut buf)).await.unwrap().unwrap();
        assert!(
            Transport::new(Mode::Aes256Gcm, KEY, 0)
                .decrypt(&buf[..n])
                .is_some()
        );
    }

    /// Heartbeats (op 3) the fake receives on `ws`, ACKing only the first; returns the count before the
    /// driver drops the socket.
    async fn beats_until_dropped(ws: &mut WebSocketStream<TcpStream>) -> usize {
        let mut beats = 0;
        loop {
            match timeout(T, ws.next()).await.expect("driver must give up on the link") {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["op"] == 3 {
                        beats += 1;
                        if beats == 1 {
                            send_json(ws, json!({"op": 6, "d": {"t": v["d"]["t"]}})).await;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return beats,
                Some(Ok(_)) => {}
            }
        }
    }

    #[tokio::test]
    async fn missed_heartbeat_acks_reconnect() {
        let mut f = start().await;
        let id = recv_json(&mut f.ws).await;
        assert_eq!(id["op"], 0);
        send_json(&mut f.ws, json!({"op": 8, "d": {"heartbeat_interval": 1000.0}})).await;
        // one ACKed beat, then two unanswered ones: the link counts as dead at the next beat
        assert_eq!(beats_until_dropped(&mut f.ws).await, 3);
        let ws = accept(&f.listener).await;
        drop(ws); // reconnected: a new socket was opened
    }

    #[tokio::test]
    async fn voice_server_change_reconnects() {
        let mut f = start().await;
        handshake(&mut f).await;
        // VOICE_SERVER_UPDATE with a new endpoint: the same driver (and its channels) moves over
        let next = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut c = conn(&next.local_addr().unwrap().to_string());
        c.token = "tok2".into();
        f.cmds.send(VoiceCmd::Restart(c, 4)).await.unwrap();
        assert!(
            matches!(timeout(T, f.ws.next()).await.unwrap(), Some(Ok(Message::Close(_))) | Some(Err(_)) | None),
            "old server's socket closed"
        );
        f.listener = next;
        f.ws = accept(&f.listener).await;
        f.udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let id = recv_json(&mut f.ws).await;
        assert_eq!((id["op"].clone(), id["d"]["token"].clone()), (json!(0), json!("tok2")), "fresh Identify with the new token");
        send_json(&mut f.ws, json!({"op": 8, "d": {"heartbeat_interval": 30000.0}})).await;
        let port = f.udp.local_addr().unwrap().port();
        send_json(&mut f.ws, json!({"op": 2, "d": {"ssrc": 5, "ip": "127.0.0.1", "port": port, "modes": ["aead_aes256_gcm_rtpsize"]}})).await;
        let mut buf = [0u8; 128];
        let (_, from) = timeout(T, f.udp.recv_from(&mut buf)).await.unwrap().unwrap();
        let mut resp = [0u8; 74];
        resp[..4].copy_from_slice(&[0, 2, 0, 70]);
        resp[8..17].copy_from_slice(b"127.0.0.1");
        resp[72..].copy_from_slice(&from.port().to_be_bytes());
        f.udp.send_to(&resp, from).await.unwrap();
        assert_eq!(recv_json(&mut f.ws).await["op"], 1);
        send_json(&mut f.ws, json!({"op": 4, "d": {"mode": "aead_aes256_gcm_rtpsize", "secret_key": KEY, "dave_protocol_version": 0}})).await;
        wait_event(&mut f.events, |e| matches!(e, VoiceEvent::Status(VoiceStatus::Connected))).await;
        f.cmds.send(VoiceCmd::Opus(vec![1])).await.unwrap();
        let n = timeout(T, f.udp.recv(&mut buf)).await.unwrap().unwrap();
        let (h, _) = Transport::new(Mode::Aes256Gcm, KEY, 0).decrypt(&buf[..n]).expect("media to the new server");
        assert_eq!(h.ssrc, 5);
    }

    #[test]
    fn voice_url_from_endpoint() {
        assert_eq!(
            voice_url("c-ams.discord.media:443"),
            "wss://c-ams.discord.media:443/?v=8"
        );
        assert_eq!(
            voice_url("wss://c-ams.discord.media/"),
            "wss://c-ams.discord.media/?v=8"
        );
    }

    #[test]
    fn heartbeat_period_at_least_1s() {
        assert_eq!(heartbeat_period(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(
            heartbeat_period(Duration::from_millis(13750)),
            Duration::from_millis(13750)
        );
    }
}
