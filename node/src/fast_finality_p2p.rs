//! Dedicated P2P propagation of SFF (Fast Finality) votes, certificates and
//! repair requests (design §5 `fast_finality_network.rs`).
//!
//! This is a small flood-fill protocol of its own, mirroring `exchange_p2p`:
//! every message is cheaply size-checked, de-duplicated and rate-limited before
//! it is decoded or validated, and only *accepted* (validated) messages are
//! relayed. Vote *signatures* are NOT checked on receipt — the pool's
//! `aggregate_votes` re-verifies every constituent vote when a QC is built, so the
//! only check here is structural sanity (plus a full BLS check for certificates).
//! Invalid traffic is never amplified.
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
pub const SFF_PROTOCOL: &str = "/sun/sff/2";

/// A single wire message. Votes and certificates carry their own signature and
/// are validated before they are accepted locally or relayed.
#[derive(Clone, Encode, Decode, PartialEq)]
pub enum SffMessage {
    Vote(Vote),
    Certificate(QuorumCertificate),
    Repair(RepairRequest),
    /// Transport-level receipt for the exact encoded message hash.
    Ack([u8; 32]),
}

const DELIVERY_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);
const DELIVERY_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(8);
const MAX_PENDING_PER_PEER: usize = 1024;
const MAX_ACK_HISTORY_PER_PEER: usize = 8192;
const MAX_CONCURRENT_BLS_VALIDATIONS: usize = 4;
const MAX_PENDING_VALIDATIONS: usize = 256;
const MAX_RETRIES_PER_TICK: usize = 4;

struct PendingDelivery {
    bytes: Vec<u8>,
    last_sent: std::time::Instant,
    attempts: u32,
}

struct PendingValidation {
    message: SffMessage,
    waiters: Vec<(sc_network::PeerId, Vec<u8>)>,
}

#[derive(Default)]
struct AckHistory {
    set: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl AckHistory {
    fn contains(&self, id: &[u8; 32]) -> bool {
        self.set.contains(id)
    }

    fn insert(&mut self, id: [u8; 32]) {
        if !self.set.insert(id) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > MAX_ACK_HISTORY_PER_PEER {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
    }
}

/// "Do you have the QC for `(set_id, height, round, phase)`?" — answered by
/// re-sending a locally known certificate once.
#[derive(Clone, Encode, Decode, PartialEq)]
pub struct RepairRequest {
    pub set_id: u64,
    pub height: u32,
    pub round: u32,
    pub phase: Phase,
    /// Distinguishes a later retry after an earlier request was ACKed as received.
    pub request_id: u32,
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
///
/// Set generously: this is a *trusted* validator-to-validator protocol, and the
/// real backstop against abuse is [`MAX_BYTES_PER_WINDOW`] (4 MiB / window).
/// The original 200 was far too low — during recovery after a transient
/// substream stall every height that lost a vote re-sends it, and the healing
/// burst (tens of messages per 10-block window, plus certificate re-gossip and
/// repair answers) routinely exceeded 200, so the rate limiter *dropped the
/// healing traffic itself*, leaving permanent holes. 1024 keeps the bound
/// meaningful while letting legitimate recovery complete.
pub const MAX_MESSAGES_PER_WINDOW: u32 = 1024;

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
    Repair(u64, u32, u32, u8, u32),
    Ack([u8; 32]),
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
    let mut pending: HashMap<sc_network::PeerId, HashMap<[u8; 32], PendingDelivery>> =
        HashMap::new();
    let mut acknowledged: HashMap<sc_network::PeerId, AckHistory> = HashMap::new();
    let mut validating: HashMap<SeenKey, PendingValidation> = HashMap::new();
    let (validated_tx, mut validated_rx) =
        tokio::sync::mpsc::channel::<(SeenKey, bool)>(MAX_PENDING_VALIDATIONS);
    let validation_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_BLS_VALIDATIONS));
    let mut retry_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    retry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut gossip_local_count: u64 = 0;
    let mut gossip_event_count: u64 = 0;
    let mut gossip_ack_count: u64 = 0;
    let mut total_iter: u64 = 0;
    let mut last_hb = std::time::Instant::now();
    let mut last_wait_ms: u64 = 0;
    let mut last_proc_ms: u64 = 0;

    loop {
        use futures::StreamExt;

        enum Which {
            Event(Option<NotificationEvent>),
            Local(Option<Vec<u8>>),
            Retry,
            Validated(Option<(SeenKey, bool)>),
        }
        let wait_start = std::time::Instant::now();
        let which = tokio::select! {
            event = service.next_event() => Which::Event(event),
            bytes = local.next() => Which::Local(bytes),
            _ = retry_tick.tick() => Which::Retry,
            validation = validated_rx.recv() => Which::Validated(validation),
        };
        last_wait_ms = wait_start.elapsed().as_millis() as u64;
        let proc_start = std::time::Instant::now();

        match which {
            Which::Local(Some(bytes)) => {
                // Diagnostic (option A): show what our own engine submits, so we can
                // confirm a node's votes actually enter its gossip loop (vs. a broken
                // engine→gossip channel) — and how fast the local channel floods.
                if let Ok(m) = SffMessage::decode(&mut &bytes[..]) {
                    match &m {
                        SffMessage::Vote(v) => log::debug!(
                            target: "sff",
                            "gossip-local: Vote set_id={} h={} round={} {:?} signer={}",
                            v.set_id, v.height, v.round, v.phase, v.signer_index
                        ),
                        SffMessage::Certificate(qc) => log::debug!(
                            target: "sff",
                            "gossip-local: Cert set_id={} h={} round={} {:?}",
                            qc.set_id, qc.height, qc.round, qc.phase
                        ),
                        SffMessage::Repair(_) => {}
                        SffMessage::Ack(_) => {}
                    }
                }
                // Local submit from our own engine. Always relay: re-transmissions
                // (re-sent votes from `maintain_votes` / `request_repairs`) are how we
                // heal votes the peer lost in gossip, so they MUST reach the peer. We
                // still record the key in `seen` so that a peer's relay of this same
                // message back to us is dropped (loop prevention) — but we never let
                // `seen` suppress our own re-transmission.
                if let Some(k) = local_seen_key(&bytes) {
                    seen.insert(k);
                }
                gossip_local_count += 1;
                relay_reliably(&mut service, &peers, &bytes, &mut pending, &acknowledged).await;
            }
            Which::Local(None) => continue,
            Which::Retry => {
                retry_pending(&mut service, &mut pending).await;
            }
            Which::Validated(Some((key, valid))) => {
                if let Some(pending_validation) = validating.remove(&key) {
                    if valid {
                        seen.insert(key);
                        accept_into_pool(&pool, &pending_validation.message);
                        let relay_bytes = pending_validation
                            .waiters
                            .first()
                            .map(|(_, bytes)| bytes.clone());
                        if let Some((first_peer, _)) = pending_validation.waiters.first() {
                            match &pending_validation.message {
                                SffMessage::Vote(v) => log::info!(
                                    target: "sff",
                                    "gossip-rcv: Vote set_id={} h={} round={} {:?} signer={} from {}",
                                    v.set_id, v.height, v.round, v.phase, v.signer_index,
                                    first_peer.to_base58()
                                ),
                                SffMessage::Certificate(qc) => log::info!(
                                    target: "sff",
                                    "gossip-rcv: Cert set_id={} h={} round={} {:?} from {}",
                                    qc.set_id, qc.height, qc.round, qc.phase,
                                    first_peer.to_base58()
                                ),
                                SffMessage::Repair(_) | SffMessage::Ack(_) => {}
                            }
                        }
                        for (peer, bytes) in &pending_validation.waiters {
                            send_ack(&mut service, peer, bytes).await;
                        }
                        if let Some(bytes) = relay_bytes {
                            relay_reliably(
                                &mut service,
                                &peers,
                                &bytes,
                                &mut pending,
                                &acknowledged,
                            )
                            .await;
                        }
                    } else {
                        log::info!(
                            target: "sff",
                            "gossip-drop: invalid/unknown-set message; waiters={}",
                            pending_validation.waiters.len()
                        );
                    }
                }
            }
            Which::Validated(None) => continue,
            Which::Event(None) => {
                log::warn!(
                    target: "sff",
                    "gossip: next_event returned None — exiting gossip loop (service closed?)"
                );
                break;
            }
            Which::Event(Some(event)) => match event {
                NotificationEvent::NotificationStreamOpened { peer, .. } => {
                    log::debug!(target: "sff", "gossip: substream opened with {peer}");
                    peers.insert(peer);
                    gossip_event_count += 1;
                }
                NotificationEvent::NotificationStreamClosed { peer } => {
                    log::debug!(target: "sff", "gossip: substream closed with {peer}");
                    peers.remove(&peer);
                    acknowledged.remove(&peer);
                    gossip_event_count += 1;
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

                    // ACKs are small transport bookkeeping and must not consume
                    // the same per-window budget as votes and certificates.
                    if !matches!(&msg, SffMessage::Ack(_)) && !limits.allow(&peer_bytes) {
                        log::debug!(target: "sff", "gossip: dropped peer message (rate limited)");
                        continue;
                    }

                    if let SffMessage::Ack(id) = &msg {
                        if let Some(deliveries) = pending.get_mut(&peer) {
                            if deliveries.remove(id).is_some() {
                                acknowledged.entry(peer.clone()).or_default().insert(*id);
                                gossip_ack_count += 1;
                                log::debug!(target: "sff", "gossip-ack: peer={} pending={}", peer, deliveries.len());
                            }
                        }
                        continue;
                    }

                    // A repair request is answered back to the asker only.
                    if let SffMessage::Repair(req) = &msg {
                        send_ack(&mut service, &peer, &notification).await;
                        if let Some(qc) = answer_repair(&pool, req) {
                            log::debug!(
                                target: "sff",
                                "gossip: repair req set_id={} height={} round={} phase={:?} answered=true",
                                req.set_id, req.height, req.round, req.phase
                            );
                            send_notification(&mut service, &peer, &qc).await;
                        } else {
                            log::debug!(
                                target: "sff",
                                "gossip: repair req set_id={} height={} round={} phase={:?} answered=false",
                                req.set_id, req.height, req.round, req.phase
                            );
                        }
                        continue;
                    }

                    let key = seen_key(&msg);
                    if seen.set.contains(&key) {
                        accept_into_pool(&pool, &msg);
                        send_ack(&mut service, &peer, &notification).await;
                        continue;
                    }

                    if let Some(pending_validation) = validating.get_mut(&key) {
                        if pending_validation.waiters.len() < 64 {
                            pending_validation.waiters.push((peer, notification));
                        }
                        continue;
                    }

                    if validating.len() >= MAX_PENDING_VALIDATIONS {
                        log::debug!(target: "sff", "gossip: validation queue full; leaving message unacked for retry");
                        continue;
                    }

                    validating.insert(
                        key.clone(),
                        PendingValidation {
                            message: msg.clone(),
                            waiters: vec![(peer, notification)],
                        },
                    );
                    let provider = provider.clone();
                    let genesis_hash = client.info().genesis_hash;
                    let slots = validation_slots.clone();
                    let tx = validated_tx.clone();
                    tokio::spawn(async move {
                        let valid = match slots.acquire_owned().await {
                            Ok(_permit) => gossip_validate(&provider, genesis_hash, &msg),
                            Err(_) => false,
                        };
                        let _ = tx.send((key, valid)).await;
                    });
                }
                _ => {}
            },
        }
        // --- loop liveness / bottleneck probe (option A) ---
        last_proc_ms = proc_start.elapsed().as_millis() as u64;
        total_iter += 1;
        let now = std::time::Instant::now();
        if now.duration_since(last_hb) > std::time::Duration::from_secs(5) {
            let pending_count: usize = pending.values().map(HashMap::len).sum();
            log::info!(
                target: "sff",
                "gossip-loop alive: iter={} local={} event={} ack={} pending={} validating={} last_wait={}ms last_proc={}ms peers={}",
                total_iter, gossip_local_count, gossip_event_count, gossip_ack_count,
                pending_count, validating.len(), last_wait_ms, last_proc_ms, peers.len()
            );
            last_hb = now;
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
        SffMessage::Repair(req) => SeenKey::Repair(
            req.set_id,
            req.height,
            req.round,
            req.phase as u8,
            req.request_id,
        ),
        SffMessage::Ack(id) => SeenKey::Ack(*id),
    }
}

/// Cheap structural sanity check for a gossip message — NO BLS.
///
/// Vote *signatures* and PoPs are intentionally NOT checked here. The pool's
/// `aggregate_votes` re-verifies every constituent vote (PoP + signature) when a QC
/// is built, so re-checking each vote on every receipt is pure waste and — under four
/// co-located validators contending for CPU — was the dominant BLS cost that stalled
/// the gossip loop. We only reject grossly malformed messages (unknown set,
/// out-of-range signer index, wrong genesis) so the pool and relay stay clean.
///
/// Certificates (QCs) ARE verified here: they are a single aggregate signature (one
/// pairing) and we must trust a delivered certificate to advance the frontier.
///
/// Trust assumption: a *trusted* validator set. On an adversarial network a forged
/// vote could occupy a signer slot before the honest one arrives and starve QC
/// formation; harden by verifying votes on receipt, or by filtering invalid votes in
/// `try_build_qc` instead of aborting the whole aggregate.
fn gossip_validate<P: AuthorityProvider>(
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
            v.signer_index < set.authorities.len() as u32
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
        SffMessage::Repair(_) | SffMessage::Ack(_) => true,
    }
}

/// Full BLS validation of a message against the candidate authority set.
///
/// Used by tests (and anywhere a definitive check is needed). The hot gossip path
/// uses the cheaper [`gossip_validate`], relying on `aggregate_votes` to re-check
/// votes at QC-build time.
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
        SffMessage::Repair(_) | SffMessage::Ack(_) => true,
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
        SffMessage::Repair(_) | SffMessage::Ack(_) => {}
    }
}

fn answer_repair(pool: &Arc<Mutex<FastFinalityPool>>, req: &RepairRequest) -> Option<Vec<u8>> {
    let p = pool.lock().ok()?;
    let qc = p.qc_for(req.set_id, req.phase, req.height, req.round)?;
    let msg = SffMessage::Certificate(qc.clone());
    Some(msg.encode())
}

async fn relay_reliably(
    service: &mut Box<dyn NotificationService>,
    peers: &HashSet<sc_network::PeerId>,
    notification: &[u8],
    pending: &mut HashMap<sc_network::PeerId, HashMap<[u8; 32], PendingDelivery>>,
    acknowledged: &HashMap<sc_network::PeerId, AckHistory>,
) {
    let bytes = notification.to_vec();
    let id = sp_core::hashing::blake2_256(&bytes);
    for peer in peers {
        if acknowledged
            .get(peer)
            .is_some_and(|history| history.contains(&id))
        {
            continue;
        }
        let queue = pending.entry(peer.clone()).or_default();
        if queue.contains_key(&id) {
            continue;
        }
        if queue.len() >= MAX_PENDING_PER_PEER {
            log::warn!(target: "sff", "gossip-outbox full for {peer}; pending={}", queue.len());
            continue;
        }
        queue.insert(
            id,
            PendingDelivery {
                bytes: bytes.clone(),
                last_sent: std::time::Instant::now(),
                attempts: 1,
            },
        );
        send_notification(service, peer, &bytes).await;
    }
}

async fn retry_pending(
    service: &mut Box<dyn NotificationService>,
    pending: &mut HashMap<sc_network::PeerId, HashMap<[u8; 32], PendingDelivery>>,
) {
    let now = std::time::Instant::now();
    let mut due = Vec::new();
    for (peer, deliveries) in pending.iter() {
        for (id, delivery) in deliveries {
            if now.duration_since(delivery.last_sent) >= retry_delay(delivery.attempts) {
                due.push((delivery.last_sent, peer.clone(), *id));
            }
        }
    }
    due.sort_by_key(|(last_sent, _, _)| *last_sent);
    due.truncate(MAX_RETRIES_PER_TICK);
    for (_, peer, id) in due {
        let Some(delivery) = pending
            .get_mut(&peer)
            .and_then(|entries| entries.get_mut(&id))
        else {
            continue;
        };
        delivery.last_sent = std::time::Instant::now();
        delivery.attempts = delivery.attempts.saturating_add(1);
        let attempts = delivery.attempts;
        let bytes = delivery.bytes.clone();
        if attempts == 2 || attempts.is_power_of_two() {
            log::debug!(target: "sff", "gossip-retry: peer={} id={} attempts={}", peer, hex(&id), attempts);
        }
        send_notification(service, &peer, &bytes).await;
    }
}

fn retry_delay(attempts: u32) -> std::time::Duration {
    let shift = attempts.saturating_sub(1).min(4);
    DELIVERY_RETRY_BASE
        .checked_mul(1u32 << shift)
        .unwrap_or(DELIVERY_RETRY_MAX)
        .min(DELIVERY_RETRY_MAX)
}

async fn send_notification(
    service: &mut Box<dyn NotificationService>,
    peer: &sc_network::PeerId,
    notification: &[u8],
) {
    let t0 = std::time::Instant::now();
    let send = service.send_async_notification(peer, notification.to_vec());
    match tokio::time::timeout(std::time::Duration::from_millis(500), send).await {
        Ok(Ok(())) => {
            let d = t0.elapsed().as_millis();
            if d > 5 {
                log::debug!(target: "sff", "gossip-send to {peer} ok {}ms", d);
            }
        }
        Ok(Err(e)) => {
            log::warn!(target: "sff", "gossip: send to {peer} failed: {e:?}");
        }
        Err(_) => {
            log::warn!(target: "sff", "gossip: send to {peer} TIMED OUT");
        }
    }
}

async fn send_ack(
    service: &mut Box<dyn NotificationService>,
    peer: &sc_network::PeerId,
    notification: &[u8],
) {
    let id = sp_core::hashing::blake2_256(notification);
    let ack = SffMessage::Ack(id).encode();
    send_notification(service, peer, &ack).await;
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
