//! Dedicated P2P propagation of SFF (Fast Finality) votes, certificates and
//! repair requests (design §5 `fast_finality_network.rs`).
//!
//! This is a small flood-fill protocol of its own, mirroring `exchange_p2p`:
//! every message is cheaply size-checked, de-duplicated and rate-limited before
//! it is decoded or validated, and only *accepted* (validated) messages are
//! relayed. Validation here is the real BLS check against the candidate
//! authority set returned by the [`AuthorityProvider`]; invalid traffic is never
//! amplified.
//!
//! F2 is **shadow**: nothing here changes fork choice, runtime state or user
//! finality. The pool is only read by the observer, which logs/meters diffs.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use sc_network::{
    config::SetConfig,
    peer_store::PeerStoreProvider,
    service::{
        traits::{NotificationEvent, NotificationService},
        NotificationMetrics,
    },
    NetworkBackend, ProtocolName,
};
use scale_codec::{Decode, Encode};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, UniqueSaturatedInto};

use sff_crypto::{verify_qc, verify_vote};
use sff_primitives::{Phase, QuorumCertificate, Vote};

use sunchain_runtime::apis::FastFinalityApi;

use crate::fast_finality::AuthorityProvider;
use crate::fast_finality_pool::FastFinalityPool;

/// Protocol name. Versioned, so it can evolve without cross-talk.
pub const SFF_PROTOCOL: &str = "/sun/sff/1";

/// A single wire message. Votes and certificates carry their own signature and
/// are validated before they are accepted locally or relayed.
#[derive(Clone, Encode, Decode, PartialEq)]
pub enum SffMessage {
    Vote(Vote),
    Certificate(QuorumCertificate),
    Repair(RepairRequest),
}

/// "Do you have the QC for `(set_id, height, round, phase)`?" — answered by
/// re-sending a locally known certificate once.
#[derive(Clone, Encode, Decode, PartialEq)]
pub struct RepairRequest {
    pub set_id: u64,
    pub height: u32,
    pub round: u32,
    pub phase: Phase,
}

/// Builds the notification-protocol config, mirroring how GRANDPA / exchange
/// register their own set.
pub fn sff_peers_set_config<B: BlockT, N: NetworkBackend<B, <B as BlockT>::Hash>>(
    protocol_name: ProtocolName,
    metrics: NotificationMetrics,
    peer_store_handle: Arc<dyn PeerStoreProvider>,
) -> (N::NotificationProtocolConfig, Box<dyn NotificationService>) {
    N::notification_config(
        protocol_name,
        Vec::new(),
        // A vote is ~200 B; a certificate a few hundred. 16 KiB is generous.
        16 * 1024,
        None,
        SetConfig {
            in_peers: 25,
            out_peers: 25,
            reserved_nodes: Vec::new(),
            non_reserved_mode: sc_network::config::NonReservedPeerMode::Accept,
        },
        metrics,
        peer_store_handle,
    )
}

/// A packet larger than this cannot be a valid SFF message; drop it before decode.
pub const MAX_NOTIFICATION_BYTES: usize = 16 * 1024;

/// Blocks per rate-limit window (advances the window on chain height, not traffic).
pub const RATE_LIMIT_WINDOW_BLOCKS: u32 = 10;

/// Messages one peer may have accepted per window — a bound, not a generosity.
pub const MAX_MESSAGES_PER_WINDOW: u32 = 200;

/// Bytes one peer may push per window.
pub const MAX_BYTES_PER_WINDOW: u64 = 4 * 1024 * 1024;

/// Malformed packets (oversized / undecodable) one peer may send per window,
/// counted separately so junk cannot spend an honest validator's budget.
pub const MAX_MALFORMED_PER_WINDOW: u32 = 64;

/// One message identified by its semantic key, not its bytes, so a re-signed
/// vote is not relayed twice and a re-relayed packet is recognised at once.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SeenKey {
    Vote(u64, u32, u32, u8, u32),
    Cert(u64, u32, u32, u8, [u8; 32]),
}

/// Bounded set of already-seen ids, evicting the oldest when full.
pub struct Seen {
    set: HashSet<SeenKey>,
    order: VecDeque<SeenKey>,
    cap: usize,
}

impl Seen {
    pub fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Returns `true` if this id was **not** seen before.
    pub fn insert(&mut self, id: SeenKey) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }
}

#[derive(Default)]
struct PeerUsage {
    bytes: u64,
    malformed: u32,
}

/// Per-peer budget per window, reset by block height.
pub struct RateLimiter {
    counts: HashMap<Vec<u8>, u32>,
    peers: HashMap<Vec<u8>, PeerUsage>,
    window: u32,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
            peers: HashMap::new(),
            window: 0,
        }
    }

    pub fn advance(&mut self, block_number: u32) {
        let window = block_number / RATE_LIMIT_WINDOW_BLOCKS;
        if window != self.window {
            self.window = window;
            self.counts.clear();
            self.peers.clear();
        }
    }

    pub fn charge(&mut self, peer: &[u8], bytes: u64, malformed: bool) -> bool {
        let usage = self.peers.entry(peer.to_vec()).or_default();
        usage.bytes = usage.bytes.saturating_add(bytes);
        if malformed {
            usage.malformed = usage.malformed.saturating_add(1);
        }
        usage.bytes <= MAX_BYTES_PER_WINDOW && usage.malformed <= MAX_MALFORMED_PER_WINDOW
    }

    pub fn allow(&mut self, peer: &[u8]) -> bool {
        let used = self.counts.entry(peer.to_vec()).or_insert(0);
        if *used >= MAX_MESSAGES_PER_WINDOW {
            return false;
        }
        *used += 1;
        true
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs the gossip loop: relay what we have, accept and relay what we are given.
pub async fn run_sff_gossip<B, C, P>(
    mut service: Box<dyn NotificationService>,
    pool: Arc<Mutex<FastFinalityPool>>,
    provider: Arc<P>,
    client: Arc<C>,
    mut local: futures::channel::mpsc::UnboundedReceiver<Vec<u8>>,
) where
    B: BlockT,
    B: BlockT<Hash = sp_core::H256>,
    C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
    C::Api: FastFinalityApi<B>,
    P: AuthorityProvider + 'static,
{
    let mut peers: HashSet<sc_network::PeerId> = HashSet::new();
    let mut seen = Seen::new(8192);
    let mut limits = RateLimiter::new();

    loop {
        use futures::{future::Either, StreamExt};

        enum Which {
            Event(Option<NotificationEvent>),
            Local(Option<Vec<u8>>),
        }
        let which =
            match futures::future::select(Box::pin(service.next_event()), Box::pin(local.next()))
                .await
            {
                Either::Left((event, _rest)) => Which::Event(event),
                Either::Right((bytes, _rest)) => Which::Local(bytes),
            };

        match which {
            Which::Local(Some(bytes)) => {
                // Local submit: dedup by semantic key, then relay once.
                let key = local_seen_key(&bytes);
                if key.map(|k| seen.insert(k)).unwrap_or(true) {
                    relay(&mut service, &peers, &bytes).await;
                }
                continue;
            }
            Which::Local(None) => continue,
            Which::Event(None) => break,
            Which::Event(Some(event)) => match event {
                NotificationEvent::NotificationStreamOpened { peer, .. } => {
                    log::debug!(target: "sff", "gossip: substream opened with {peer}");
                    peers.insert(peer);
                }
                NotificationEvent::NotificationStreamClosed { peer } => {
                    log::debug!(target: "sff", "gossip: substream closed with {peer}");
                    peers.remove(&peer);
                }
                NotificationEvent::NotificationReceived { peer, notification } => {
                    let peer_bytes = peer.to_bytes();
                    let block_number: u32 = client.info().best_number.unique_saturated_into();
                    limits.advance(block_number);

                    let oversized = notification.len() > MAX_NOTIFICATION_BYTES;
                    if !limits.charge(&peer_bytes, notification.len() as u64, oversized) {
                        continue;
                    }
                    if oversized {
                        continue;
                    }

                    let mut input = &notification[..];
                    let Some(msg): Option<SffMessage> = Decode::decode(&mut input).ok() else {
                        // Not an SFF message: charged as malformed, never relayed.
                        limits.charge(&peer_bytes, 0, true);
                        continue;
                    };
                    if !input.is_empty() {
                        limits.charge(&peer_bytes, 0, true);
                        continue;
                    }

                    if !limits.allow(&peer_bytes) {
                        continue;
                    }

                    // A repair request is answered back to the asker only.
                    if let SffMessage::Repair(req) = &msg {
                        if let Some(qc) = answer_repair(&pool, req) {
                            let _ = service.send_async_notification(&peer, qc).await;
                        }
                        continue;
                    }

                    let key = seen_key(&msg);
                    if seen.set.contains(&key) {
                        continue; // already seen, never relayed twice
                    }

                    if !validate(&provider, client.info().genesis_hash, &msg) {
                        // Invalid (bad signature / set / unknown set) — drop.
                        continue;
                    }

                    // Invalid traffic cannot reserve a valid message's dedup key.
                    seen.insert(key);

                    // Accept into the local pool so the engine can aggregate/observe.
                    accept_into_pool(&pool, &msg);

                    relay(&mut service, &peers, &notification).await;
                }
                _ => {}
            },
        }
    }
}

/// Derives the dedup key for a locally-submitted (already-decoded-agnostic)
/// packet: try to decode; if it fails, relay once (peers drop it).
fn local_seen_key(bytes: &[u8]) -> Option<SeenKey> {
    let msg: SffMessage = Decode::decode(&mut &bytes[..]).ok()?;
    Some(seen_key(&msg))
}

fn seen_key(msg: &SffMessage) -> SeenKey {
    match msg {
        SffMessage::Vote(v) => {
            SeenKey::Vote(v.set_id, v.height, v.round, v.phase as u8, v.signer_index)
        }
        SffMessage::Certificate(qc) => SeenKey::Cert(
            qc.set_id,
            qc.height,
            qc.round,
            qc.phase as u8,
            qc.vote_payload_hash.0,
        ),
        SffMessage::Repair(_) => SeenKey::Cert(0, 0, 0, 255, [0u8; 32]),
    }
}

/// BLS-validates a message against the candidate authority set.
fn validate<P: AuthorityProvider>(
    provider: &Arc<P>,
    genesis_hash: sp_core::H256,
    msg: &SffMessage,
) -> bool {
    match msg {
        SffMessage::Vote(v) => {
            if v.genesis_hash != genesis_hash {
                return false;
            }
            let Some(set) = provider.authorities(v.set_id) else {
                return false;
            };
            // Off-chain PoP re-validation of the signer's key is the node's job;
            // `verify_vote` confirms signature + domain + set binding.
            verify_vote(v, &set).is_ok()
        }
        SffMessage::Certificate(qc) => {
            if qc.genesis_hash != genesis_hash {
                return false;
            }
            let Some(set) = provider.authorities(qc.set_id) else {
                return false;
            };
            verify_qc(qc, &set).is_ok()
        }
        SffMessage::Repair(_) => true,
    }
}

fn accept_into_pool(pool: &Arc<Mutex<FastFinalityPool>>, msg: &SffMessage) {
    let mut p = match pool.lock() {
        Ok(p) => p,
        Err(_) => return,
    };
    match msg {
        SffMessage::Vote(v) => {
            p.add_vote(v.clone());
        }
        SffMessage::Certificate(qc) => {
            p.add_qc(qc.clone());
        }
        SffMessage::Repair(_) => {}
    }
}

fn answer_repair(pool: &Arc<Mutex<FastFinalityPool>>, req: &RepairRequest) -> Option<Vec<u8>> {
    let p = pool.lock().ok()?;
    let qc = p.qc_for(req.set_id, req.phase, req.height, req.round)?;
    let msg = SffMessage::Certificate(qc.clone());
    Some(msg.encode())
}

async fn relay(
    service: &mut Box<dyn NotificationService>,
    peers: &HashSet<sc_network::PeerId>,
    notification: &[u8],
) {
    for peer in peers {
        let _ = service
            .send_async_notification(peer, notification.to_vec())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sff_crypto::{pop_sign, sign, BlsKeypair};
    use sff_primitives::{
        BlsSignature, FinalityAuthority, FinalityAuthoritySet, SFF_PROTOCOL_VERSION,
    };
    use sp_core::H256;

    struct Provider(FinalityAuthoritySet);

    impl AuthorityProvider for Provider {
        fn current_set_id(&self) -> Option<u64> {
            Some(self.0.set_id)
        }
        fn authorities(&self, set_id: u64) -> Option<FinalityAuthoritySet> {
            (set_id == self.0.set_id).then(|| self.0.clone())
        }
    }

    fn signed_vote(genesis_hash: H256) -> (Provider, Vote) {
        let keypair = BlsKeypair::from_seed(&[7; 32]);
        let set = FinalityAuthoritySet::new(
            1,
            0,
            vec![FinalityAuthority {
                validator_id: H256::repeat_byte(1),
                grandpa_id: H256::repeat_byte(2),
                bls_public_key: keypair.public(),
                proof_of_possession: pop_sign(&keypair),
                stake: 1,
            }],
        )
        .expect("one authority is valid");
        let mut vote = Vote {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash,
            set_id: set.set_id,
            snapshot_root: set.snapshot_root,
            height: 1,
            round: 0,
            phase: Phase::Prepare,
            block_hash: Some(H256::repeat_byte(3)),
            parent_hash: H256::repeat_byte(4),
            prepare_qc_hash: None,
            signer_index: 0,
            signature: BlsSignature::ZERO,
        };
        vote.signature = sign(&keypair, &vote.canonical_bytes());
        (Provider(set), vote)
    }

    #[test]
    fn validation_rejects_a_valid_signature_from_another_genesis() {
        let genesis = H256::repeat_byte(8);
        let (provider, vote) = signed_vote(genesis);
        assert!(validate(
            &Arc::new(provider),
            genesis,
            &SffMessage::Vote(vote.clone())
        ));

        let (provider, foreign_vote) = signed_vote(H256::repeat_byte(9));
        assert!(!validate(
            &Arc::new(provider),
            genesis,
            &SffMessage::Vote(foreign_vote)
        ));
    }
}

/// Peer ids are 32-byte libp2p keys; hex keeps the log readable.
#[allow(dead_code)]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
