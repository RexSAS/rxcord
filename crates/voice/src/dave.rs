//! DAVE (end-to-end media encryption) on top of `davey`: consumes `DaveIn`, produces `DaveOut`.
use crate::session::{DaveIn, DaveOut};
use davey::{MediaType, ProposalsOperationType};
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU16;

pub struct Dave {
    session: Option<davey::DaveSession>,
    user_id: u64,
    channel_id: u64,
    recognized: Vec<u64>,
    /// Current protocol version; 0 = DAVE off (media passes through).
    version: u16,
    /// Prepared transitions: id -> protocol version they switch to.
    pending: HashMap<u16, u16>,
}

// davey's session holds MLS secrets: print state only.
impl fmt::Debug for Dave {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dave")
            .field("version", &self.version)
            .field(
                "ready",
                &self.session.as_ref().is_some_and(|s| s.is_ready()),
            )
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl Dave {
    pub fn new(user_id: u64, channel_id: u64) -> Self {
        Dave {
            session: None,
            user_id,
            channel_id,
            recognized: vec![user_id],
            version: 0,
            pending: HashMap::new(),
        }
    }

    /// (Re)creates the MLS session for `version` and returns our key package; 0 turns DAVE off.
    pub fn start(&mut self, version: u16) -> Vec<DaveOut> {
        self.version = version;
        self.pending.clear();
        let Some(v) = NonZeroU16::new(version) else {
            return vec![]; // session (if any) kept: reinit on re-upgrade keeps the external sender
        };
        let init = match self.session.as_mut() {
            Some(s) => s
                .reinit(v, self.user_id, self.channel_id, None)
                .map_err(|e| e.to_string()),
            None => davey::DaveSession::new(v, self.user_id, self.channel_id, None)
                .map(|s| self.session = Some(s))
                .map_err(|e| e.to_string()),
        };
        if let Err(e) = init {
            // version stays > 0 without a session, so media is dropped, never sent in clear
            tracing::debug!("dave init v{version} failed: {e}");
            self.session = None;
            return vec![];
        }
        match self.session.as_mut().map(|s| s.create_key_package()) {
            Some(Ok(kp)) => vec![DaveOut::KeyPackage(kp)],
            Some(Err(e)) => {
                tracing::debug!("dave key package failed: {e}");
                vec![]
            }
            None => vec![],
        }
    }

    /// Never panics on bad server data: failures either re-key or are logged and ignored.
    pub fn handle(&mut self, ev: DaveIn) -> Vec<DaveOut> {
        if let DaveIn::PrepareEpoch { epoch, version } = ev {
            return if epoch == 1 {
                self.start(version)
            } else {
                vec![]
            };
        }
        if let DaveIn::PrepareTransition { id, version } = ev {
            return self.prepare(id, version);
        }
        let Some(s) = self.session.as_mut() else {
            tracing::debug!("dave event without a session: {ev:?}");
            return vec![];
        };
        match ev {
            DaveIn::ExternalSender(d) => {
                if let Err(e) = s.set_external_sender(&d) {
                    tracing::debug!("dave external sender: {e}");
                }
                vec![]
            }
            DaveIn::Proposals { op, data } => {
                let op = match op {
                    0 => ProposalsOperationType::APPEND,
                    1 => ProposalsOperationType::REVOKE,
                    _ => {
                        tracing::debug!("dave proposals: unknown op {op}");
                        return vec![];
                    }
                };
                match s.process_proposals(op, &data, Some(&self.recognized)) {
                    Ok(Some(cw)) => {
                        let mut b = cw.commit;
                        b.extend(cw.welcome.unwrap_or_default());
                        vec![DaveOut::CommitWelcome(b)]
                    }
                    Ok(None) => vec![],
                    Err(e) => {
                        tracing::debug!("dave proposals: {e}");
                        vec![]
                    }
                }
            }
            DaveIn::AnnounceCommit { id, commit } => match s.process_commit(&commit) {
                Ok(true) => {
                    let v = s.protocol_version().get();
                    self.prepare(id, v)
                }
                Ok(false) => vec![],
                Err(e) => {
                    tracing::debug!("dave commit {id}: {e}");
                    self.invalid(id)
                }
            },
            DaveIn::Welcome { id, welcome } => {
                match s.process_welcome(&welcome, Some(&self.recognized)) {
                    Ok(()) => {
                        let v = s.protocol_version().get();
                        self.prepare(id, v)
                    }
                    Err(e) => {
                        tracing::debug!("dave welcome {id}: {e}");
                        self.invalid(id)
                    }
                }
            }
            DaveIn::ExecuteTransition(id) => {
                if let Err(e) = s.execute_transition(id) {
                    tracing::debug!("dave execute {id}: {e}");
                }
                if let Some(v) = self.pending.remove(&id) {
                    self.version = v;
                }
                vec![]
            }
            DaveIn::PrepareEpoch { .. } | DaveIn::PrepareTransition { .. } => vec![], // handled above
        }
    }

    /// Transition 0 applies at once; any other id waits for ExecuteTransition.
    fn prepare(&mut self, id: u16, version: u16) -> Vec<DaveOut> {
        if let Some(Err(e)) = self
            .session
            .as_mut()
            .map(|s| s.prepare_transition(id, version))
        {
            tracing::debug!("dave prepare {id}: {e}");
        }
        if id == 0 {
            self.version = version;
            return vec![];
        }
        self.pending.insert(id, version);
        vec![DaveOut::TransitionReady(id)]
    }

    fn invalid(&mut self, id: u16) -> Vec<DaveOut> {
        let mut outs = vec![DaveOut::InvalidCommitWelcome(id)];
        outs.extend(self.start(self.version));
        outs
    }

    /// Users expected in the group (proposals/welcomes naming anyone else are rejected); we are always included.
    pub fn set_members(&mut self, mut ids: Vec<u64>) {
        if !ids.contains(&self.user_id) {
            ids.push(self.user_id);
        }
        self.recognized = ids;
    }

    /// None = not ready: drop the frame. Copies the frame unchanged only when DAVE is off.
    pub fn encrypt(&mut self, opus: &[u8]) -> Option<Vec<u8>> {
        if self.version == 0 {
            return Some(opus.to_vec());
        }
        self.session
            .as_mut()?
            .encrypt_opus(opus)
            .ok()
            .map(|c| c.into_owned())
    }

    /// None = undecryptable: drop the frame, never play it.
    pub fn decrypt(&mut self, user_id: u64, frame: &[u8]) -> Option<Vec<u8>> {
        if self.version == 0 {
            return Some(frame.to_vec());
        }
        self.session
            .as_mut()?
            .decrypt(user_id, MediaType::AUDIO, frame)
            .ok()
    }

    pub fn privacy_code(&self) -> Option<String> {
        self.session
            .as_ref()?
            .voice_privacy_code()
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openmls::prelude::{
        tls_codec::{DeserializeBytes as _, Serialize as _},
        *,
    };
    use openmls_basic_credential::SignatureKeyPair;
    use openmls_rust_crypto::OpenMlsRustCrypto;

    const CH: u64 = 1234567890;
    const A: u64 = 1111;
    const B: u64 = 2222;

    /// Plays the voice server's MLS external sender (as davey's own tests do).
    struct Server {
        provider: OpenMlsRustCrypto,
        signer: SignatureKeyPair,
        sender: ExternalSender,
    }

    impl Server {
        fn new() -> Self {
            let suite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
            let signer = SignatureKeyPair::new(suite.signature_algorithm()).unwrap();
            let sender = ExternalSender::new(
                signer.public().into(),
                BasicCredential::new(vec![0, 1, 1, 0]).into(),
            );
            Server {
                provider: OpenMlsRustCrypto::default(),
                signer,
                sender,
            }
        }

        fn external_sender(&self) -> DaveIn {
            DaveIn::ExternalSender(self.sender.tls_serialize_detached().unwrap())
        }

        /// Op 27 append proposal adding the owner of `key_package` at epoch 0.
        fn propose_add(&self, key_package: &[u8]) -> DaveIn {
            let kp = KeyPackageIn::tls_deserialize_exact_bytes(key_package)
                .unwrap()
                .validate(self.provider.crypto(), ProtocolVersion::Mls10)
                .unwrap();
            let msg = ExternalProposal::new_add::<OpenMlsRustCrypto>(
                kp,
                GroupId::from_slice(&CH.to_be_bytes()),
                GroupEpoch::from(0),
                &self.signer,
                SenderExtensionIndex::new(0),
            )
            .unwrap();
            let data = VLBytes::new(msg.tls_serialize_detached().unwrap())
                .tls_serialize_detached()
                .unwrap();
            DaveIn::Proposals { op: 0, data }
        }
    }

    fn key_package(outs: &[DaveOut]) -> Vec<u8> {
        match outs {
            [DaveOut::KeyPackage(kp)] => kp.clone(),
            _ => panic!("expected one KeyPackage"),
        }
    }

    fn started(user: u64, server: &Server) -> (Dave, Vec<u8>) {
        let mut d = Dave::new(user, CH);
        d.set_members(vec![A, B]);
        let kp = key_package(&d.start(1));
        assert!(d.handle(server.external_sender()).is_empty());
        (d, kp)
    }

    /// Splits an op 28 payload into (commit MLSMessage, welcome).
    fn split(cw: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (_, rest) = MlsMessageIn::tls_deserialize_bytes(cw).unwrap();
        let n = cw.len() - rest.len();
        (cw[..n].to_vec(), rest.to_vec())
    }

    #[test]
    fn version_zero_passthrough() {
        let mut d = Dave::new(A, CH);
        assert!(d.start(0).is_empty());
        assert_eq!(d.encrypt(&[1, 2, 3]), Some(vec![1, 2, 3]));
        assert_eq!(d.decrypt(B, &[4, 5]), Some(vec![4, 5]));
        assert_eq!(d.privacy_code(), None);
    }

    #[test]
    fn start_emits_key_package() {
        let mut d = Dave::new(A, CH);
        assert!(!key_package(&d.start(1)).is_empty());
    }

    #[test]
    fn not_ready_drops() {
        let mut d = Dave::new(A, CH);
        d.start(1);
        assert_eq!(d.encrypt(&[1, 2, 3]), None);
        assert_eq!(d.decrypt(B, &[1, 2, 3]), None);
    }

    #[test]
    fn unsupported_version_never_passes_through() {
        let mut d = Dave::new(A, CH);
        assert!(d.start(99).is_empty());
        assert_eq!(d.encrypt(&[1, 2, 3]), None);
        assert_eq!(d.decrypt(B, &[1, 2, 3]), None);
    }

    #[test]
    fn bad_commit_rekeys() {
        let server = Server::new();
        let (mut a, _) = started(A, &server);
        let (_, kp_b) = started(B, &server);
        // a has a commit pending, so the garbage commit is actually parsed and rejected
        assert!(matches!(
            &a.handle(server.propose_add(&kp_b))[..],
            [DaveOut::CommitWelcome(_)]
        ));
        let outs = a.handle(DaveIn::AnnounceCommit {
            id: 7,
            commit: vec![0xde, 0xad],
        });
        assert!(
            matches!(&outs[..], [DaveOut::InvalidCommitWelcome(7), DaveOut::KeyPackage(kp)] if !kp.is_empty())
        );
    }

    #[test]
    fn bad_welcome_rekeys() {
        let mut d = Dave::new(A, CH);
        d.start(1);
        let outs = d.handle(DaveIn::Welcome {
            id: 3,
            welcome: vec![1, 2, 3],
        });
        assert!(matches!(
            &outs[..],
            [DaveOut::InvalidCommitWelcome(3), DaveOut::KeyPackage(_)]
        ));
    }

    #[test]
    fn garbage_never_panics() {
        let mut d = Dave::new(A, CH);
        d.start(1);
        for ev in [
            DaveIn::ExternalSender(vec![1, 2]),
            DaveIn::Proposals {
                op: 0,
                data: vec![9; 5],
            },
            DaveIn::Proposals {
                op: 7,
                data: vec![],
            },
            DaveIn::ExecuteTransition(42),
        ] {
            assert!(d.handle(ev).is_empty());
        }
    }

    #[test]
    fn transition_zero_executes() {
        let mut d = Dave::new(A, CH);
        d.start(1);
        assert!(
            d.handle(DaveIn::PrepareTransition { id: 0, version: 1 })
                .is_empty()
        );
        assert!(matches!(
            &d.handle(DaveIn::PrepareTransition { id: 5, version: 1 })[..],
            [DaveOut::TransitionReady(5)]
        ));
    }

    #[test]
    fn downgrade_to_zero_passes_through_after_execute() {
        let mut d = Dave::new(A, CH);
        d.start(1);
        d.handle(DaveIn::PrepareTransition { id: 9, version: 0 });
        assert_eq!(d.encrypt(&[1]), None);
        d.handle(DaveIn::ExecuteTransition(9));
        assert_eq!(d.encrypt(&[1]), Some(vec![1]));
    }

    #[test]
    fn prepare_epoch_one_rekeys() {
        let mut d = Dave::new(A, CH);
        d.start(0);
        let outs = d.handle(DaveIn::PrepareEpoch {
            epoch: 1,
            version: 1,
        });
        assert!(!key_package(&outs).is_empty());
        assert_eq!(d.encrypt(&[1]), None);
    }

    #[test]
    fn two_members_round_trip() {
        let server = Server::new();
        let (mut a, _) = started(A, &server);
        let (mut b, kp_b) = started(B, &server);

        let cw = match &a.handle(server.propose_add(&kp_b))[..] {
            [DaveOut::CommitWelcome(cw)] => cw.clone(),
            _ => panic!("expected CommitWelcome"),
        };
        let (commit, welcome) = split(&cw);
        assert!(!welcome.is_empty());

        assert!(matches!(
            &a.handle(DaveIn::AnnounceCommit { id: 1, commit })[..],
            [DaveOut::TransitionReady(1)]
        ));
        assert!(matches!(
            &b.handle(DaveIn::Welcome { id: 1, welcome })[..],
            [DaveOut::TransitionReady(1)]
        ));
        assert!(a.handle(DaveIn::ExecuteTransition(1)).is_empty());
        assert!(b.handle(DaveIn::ExecuteTransition(1)).is_empty());

        let opus = [0x78, 1, 2, 3, 4, 5, 6, 7];
        let sealed = a.encrypt(&opus).unwrap();
        assert_ne!(sealed, opus);
        assert_eq!(b.decrypt(A, &sealed).unwrap(), opus);
        assert_eq!(b.decrypt(B, &sealed), None);
        assert!(a.privacy_code().is_some());
        assert_eq!(a.privacy_code(), b.privacy_code());
    }
}
