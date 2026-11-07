//! RTP packets, rtpsize AEAD transport encryption and UDP IP discovery.
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::fmt;

const TAG: usize = 16;
const CTR: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Aes256Gcm,
    XChaCha20,
}

impl Mode {
    pub fn from_name(s: &str) -> Option<Mode> {
        match s {
            "aead_aes256_gcm_rtpsize" => Some(Mode::Aes256Gcm),
            "aead_xchacha20_poly1305_rtpsize" => Some(Mode::XChaCha20),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Mode::XChaCha20 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    /// AES-GCM if offered, else XChaCha20; anything else is unsupported.
    pub fn choose(offered: &[String]) -> Option<Mode> {
        [Mode::Aes256Gcm, Mode::XChaCha20]
            .into_iter()
            .find(|m| offered.iter().any(|o| o == m.name()))
    }
}

pub struct Transport {
    mode: Mode,
    key: [u8; 32],
    counter: u32,
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transport")
            .field("mode", &self.mode)
            .field("key", &"<redacted>")
            .field("counter", &self.counter)
            .finish()
    }
}

impl Transport {
    pub fn new(mode: Mode, key: [u8; 32], counter: u32) -> Self {
        Self { mode, key, counter }
    }

    fn seal(&self, counter: u32, aad: &[u8], msg: &[u8]) -> Option<Vec<u8>> {
        let p = Payload { msg, aad };
        let mut n = [0u8; 24];
        n[..4].copy_from_slice(&counter.to_be_bytes());
        match self.mode {
            Mode::Aes256Gcm => Aes256Gcm::new(&self.key.into())
                .encrypt(Nonce::from_slice(&n[..12]), p)
                .ok(),
            Mode::XChaCha20 => XChaCha20Poly1305::new(&self.key.into())
                .encrypt(XNonce::from_slice(&n), p)
                .ok(),
        }
    }

    fn open(&self, counter: u32, aad: &[u8], msg: &[u8]) -> Option<Vec<u8>> {
        let p = Payload { msg, aad };
        let mut n = [0u8; 24];
        n[..4].copy_from_slice(&counter.to_be_bytes());
        match self.mode {
            Mode::Aes256Gcm => Aes256Gcm::new(&self.key.into())
                .decrypt(Nonce::from_slice(&n[..12]), p)
                .ok(),
            Mode::XChaCha20 => XChaCha20Poly1305::new(&self.key.into())
                .decrypt(XNonce::from_slice(&n), p)
                .ok(),
        }
    }

    /// Full UDP packet: header | ciphertext | tag | counter (BE).
    pub fn encrypt(&mut self, h: &RtpHeader, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0x80, 0x78];
        pkt.extend_from_slice(&h.seq.to_be_bytes());
        pkt.extend_from_slice(&h.timestamp.to_be_bytes());
        pkt.extend_from_slice(&h.ssrc.to_be_bytes());
        let ct = self
            .seal(self.counter, &pkt, payload)
            .expect("AEAD encryption cannot fail for in-memory buffers");
        pkt.extend_from_slice(&ct);
        pkt.extend_from_slice(&self.counter.to_be_bytes());
        self.counter = self.counter.wrapping_add(1);
        pkt
    }

    /// Returns the header and payload with any encrypted extension body stripped.
    pub fn decrypt(&self, packet: &[u8]) -> Option<(RtpHeader, Vec<u8>)> {
        if packet.len() < 12 + TAG + CTR || packet[0] >> 6 != 2 {
            return None;
        }
        let mut hlen = 12 + 4 * (packet[0] & 0x0f) as usize;
        let ext = packet[0] & 0x10 != 0;
        if ext {
            hlen += 4;
        }
        if packet.len() < hlen + TAG + CTR {
            return None;
        }
        let ext_body = if ext {
            4 * u16::from_be_bytes([packet[hlen - 2], packet[hlen - 1]]) as usize
        } else {
            0
        };
        let (rest, ctr) = packet.split_at(packet.len() - CTR);
        let counter = u32::from_be_bytes(ctr.try_into().ok()?);
        let plain = self.open(counter, &rest[..hlen], &rest[hlen..])?;
        let payload = plain.get(ext_body..)?.to_vec();
        let header = RtpHeader {
            seq: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes(packet[4..8].try_into().ok()?),
            ssrc: u32::from_be_bytes(packet[8..12].try_into().ok()?),
        };
        Some((header, payload))
    }
}

pub fn discovery_request(ssrc: u32) -> [u8; 74] {
    let mut b = [0u8; 74];
    b[..2].copy_from_slice(&1u16.to_be_bytes());
    b[2..4].copy_from_slice(&70u16.to_be_bytes());
    b[4..8].copy_from_slice(&ssrc.to_be_bytes());
    b
}

pub fn discovery_response(buf: &[u8]) -> Option<(String, u16)> {
    if buf.len() < 74 || buf[..2] != [0, 2] {
        return None;
    }
    let addr = &buf[8..72];
    let end = addr.iter().position(|&c| c == 0)?;
    let ip = std::str::from_utf8(&addr[..end]).ok()?.to_string();
    Some((ip, u16::from_be_bytes([buf[72], buf[73]])))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: RtpHeader = RtpHeader { seq: 7, timestamp: 960, ssrc: 0xdead_beef };

    fn round_trip(mode: Mode) {
        let mut t = Transport::new(mode, [9; 32], 5);
        let p1 = t.encrypt(&H, b"opus frame");
        let p2 = t.encrypt(&H, b"x");
        assert_eq!(&p1[p1.len() - 4..], &5u32.to_be_bytes());
        assert_eq!(&p2[p2.len() - 4..], &6u32.to_be_bytes());
        assert_eq!(t.decrypt(&p1), Some((H, b"opus frame".to_vec())));
        assert_eq!(t.decrypt(&p2), Some((H, b"x".to_vec())));
    }

    #[test]
    fn aes_round_trip() {
        round_trip(Mode::Aes256Gcm);
    }

    #[test]
    fn xchacha_round_trip() {
        round_trip(Mode::XChaCha20);
    }

    #[test]
    fn decrypt_rejects_tamper() {
        let mut t = Transport::new(Mode::Aes256Gcm, [1; 32], 0);
        let mut p = t.encrypt(&H, b"hello world");
        p[13] ^= 1;
        assert_eq!(t.decrypt(&p), None);
    }

    #[test]
    fn extension_body_is_encrypted() {
        for mode in [Mode::Aes256Gcm, Mode::XChaCha20] {
            let t = Transport::new(mode, [3; 32], 0);
            let mut aad = vec![0x90, 0x78, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
            aad.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]);
            let mut pkt = aad.clone();
            pkt.extend(t.seal(42, &aad, b"EXTBaudio").unwrap());
            pkt.extend_from_slice(&42u32.to_be_bytes());
            let (h, pl) = t.decrypt(&pkt).unwrap();
            assert_eq!((h.seq, h.ssrc), (1, 3));
            assert_eq!(pl, b"audio");
        }
    }

    #[test]
    fn rejects_short_and_non_rtp() {
        let mut t = Transport::new(Mode::Aes256Gcm, [1; 32], 0);
        assert_eq!(t.decrypt(&[0x80; 31]), None);
        let mut p = t.encrypt(&H, b"abc");
        p[0] = 0x40;
        assert_eq!(t.decrypt(&p), None);
        // CSRC count claims more header than the packet holds
        let mut p = t.encrypt(&H, b"abc");
        p[0] = 0x8f;
        assert_eq!(t.decrypt(&p), None);
    }

    #[test]
    fn discovery_layout() {
        let r = discovery_request(0x0102_0304);
        assert_eq!(r.len(), 74);
        assert_eq!(r[..8], [0, 1, 0, 0x46, 1, 2, 3, 4]);
    }

    #[test]
    fn discovery_parse() {
        let mut b = [0u8; 74];
        b[1] = 2;
        b[8..16].copy_from_slice(b"1.2.3.4\0");
        b[72..].copy_from_slice(&50000u16.to_be_bytes());
        assert_eq!(discovery_response(&b), Some(("1.2.3.4".into(), 50000)));
        assert_eq!(discovery_response(&b[..70]), None);
    }

    #[test]
    fn mode_choice() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (a, x) = (Mode::Aes256Gcm.name(), Mode::XChaCha20.name());
        assert_eq!(Mode::choose(&v(&[x, a])), Some(Mode::Aes256Gcm));
        assert_eq!(Mode::choose(&v(&[x, "other"])), Some(Mode::XChaCha20));
        assert_eq!(Mode::choose(&v(&["other"])), None);
        assert_eq!(Mode::from_name(a), Some(Mode::Aes256Gcm));
    }
}
