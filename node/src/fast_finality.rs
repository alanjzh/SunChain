//! Sunchain Fast Finality (SFF) F2 shadow engine: voter, aggregator, observer
//! and their orchestration (design §5 `fast_finality_voter.rs`,
//! `fast_finality_aggregator.rs`, `fast_finality_observer.rs`).
//!
//! **On the currently implemented genesis profile (GRANDPA finality), F2 is
//! strictly shadow.** The engine produces SFF votes + certificates, but they
//! never reach fork choice, runtime state or user finality. The only outward
//! effect is logging / metering of how SFF's fast confirmation compares to
//! GRANDPA's authoritative finality. Any SFF fault (timeout, bad certificate,
//! equivocation, network split) is logged and rate-limited here — it must never
//! interrupt BABE authoring or GRANDPA finality.
//!
//! This shadow property is **profile-dependent, not unconditional**: per
//! `docs/Consensus/Stage-2/design/Sunchain-Consensus-Genesis-Composition-Strategy.md`, consensus is chosen
//! at genesis. Selecting SFF as the finality axis (Stage 3) makes CommitQCs the
//! authoritative finality proof; that path needs a justification-import pipeline,
//! `apply_finality`, fork-choice locking, an SFF-owned set id and an essential
//! voter, and is not implemented yet. Until then SFF must remain non-essential.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{Stream, StreamExt};
use sc_client_api::{BlockImportNotification, HeaderBackend};
use scale_codec::Encode;
use sp_api::ProvideRuntimeApi;
use sp_core::H256;
use sp_keystore::{Keystore, KeystorePtr};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, UniqueSaturatedInto};
use sunchain_runtime::apis::FastFinalityApi;
use tokio::{sync::Notify, time::interval};

use sff_crypto::{pop_verify, verify};
use sff_primitives::{
    BlsPublicKey, BlsSignature, FinalityAuthoritySet, Phase, Vote, SFF_PROTOCOL_VERSION,
};

use crate::fast_finality_p2p::{RepairRequest, SffMessage};
use crate::fast_finality_pool::FastFinalityPool;
use crate::fast_finality_view::{ChainView, ClientChainView, Consistency, FastFinalityView};

/// Supplies the candidate SFF authority set. The runtime API returns the set
/// built from elected validators + registered BLS keys; this trait decouples the
/// engine from the concrete runtime API so it can be unit-tested in isolation.
pub trait AuthorityProvider: Send + Sync {
    /// Current GRANDPA/`set_id` that SFF reuses.
    fn current_set_id(&self) -> Option<u64>;
    /// Candidate authority set for `set_id`, or `None` if the set is unknown or
    /// not fully registered.
    fn authorities(&self, set_id: u64) -> Option<FinalityAuthoritySet>;
}

/// Signs SFF payloads while keeping private key material inside its backend.
pub trait SffSigner: Send + Sync {
    fn public_keys(&self) -> Vec<BlsPublicKey>;
    fn sign(&self, public: &BlsPublicKey, payload: &[u8]) -> Option<BlsSignature>;
}

/// Minimum spacing between two "SFF is dormant" warnings.
const DORMANT_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Cap on how many heights `maintain_votes` re-sends per healing tick.
///
/// Each healing tick handles a bounded batch, starting at the oldest unresolved
/// height. This bounds the outbound storm without starving the hole that blocks
/// the contiguous fast-confirmation frontier.
const MAX_REPAIR_WINDOW: u32 = 4;

/// How many blocks a `(height, round)` may wait for its PrepareQC before the
/// engine advances the round and emits a `Skip` (§13.3 P1-5).
///
/// This is deliberately *generous* (32 blocks). A healthy height forms its
/// PrepareQC in 1–2 blocks, so it is never advanced spuriously. The only thing
/// that can keep a height unconfirmed past a few blocks is a *lost* vote, and
/// that is healed by `maintain_votes` re-sends (every `REPAIR_INTERVAL_BLOCKS`)
/// and `request_repairs` (`REPAIR_REQUEST_INTERVAL_BLOCKS`) plus `repush_certificates`
/// re-gossip — all of which redeliver the missing vote/certificate within a
/// handful of blocks. Setting this too low (the old 8) meant a transient loss
/// would trip a `Skip` *before* the healing redelivery arrived, cascading into a
/// round-advancement / Skip storm and leaving SFF permanently ~24 blocks behind
/// GRANDPA. A large timeout lets healing win the race, so rounds essentially
/// never advance in steady state; genuine stalls still get a `Skip` eventually.
const ROUND_TIMEOUT_BLOCKS: u32 = 32;

/// Minimum number of blocks between two `RepairRequest`s / vote re-sends for the
/// same `(set, height, phase)`. Repairs are only sent for heights that are
/// actually missing a certificate (P1-6), so this just bounds the retry cadence.
/// Keep periodic catch-up bounded while the reliable P2P outbox retries individual
/// messages. A four-block cadence avoids flooding the local gossip queue with the
/// same votes while still repairing a missed height well inside the round timeout.
const REPAIR_INTERVAL_BLOCKS: u32 = 4;
/// Re-ask for a certificate four blocks after an ACKed request that found none.
const REPAIR_REQUEST_INTERVAL_BLOCKS: u32 = 4;

/// Re-gossip of *certificates* (push-based healing, P1-6 belt-and-suspenders).
/// A pushed certificate heals a height in a single hop (no vote round-trip), so a
/// moderately frequent re-gossip keeps the served frontier tracking GRANDPA even
/// when the point-to-point vote/re-pair path is lossy. Kept at 4 (not 1) because
/// it is heavier than vote re-sends; periodic vote re-sends cover the common
/// case, this mop-up handles the rest.
const REPUSH_INTERVAL_BLOCKS: u32 = 4;

/// Per-height round state for round advancement (§13.3 P1-5). `round` is no
/// longer hard-coded to 0: a height whose PrepareQC cannot form (lost vote,
/// transient fork) advances rounds instead of stalling forever.
#[derive(Default, Clone)]
struct RoundState {
    /// Round we are currently trying to gather a PrepareQC for.
    current_round: u32,
    /// Round whose PrepareQC has formed — once set we commit and never advance.
    prepare_done: Option<u32>,
    /// The block we are voting to prepare (the local new-best at this height).
    block_hash: H256,
    parent_hash: H256,
    /// `best` height when we first prepared this height (round 0). Round advancement
    /// is timed from *this*, not from `best - height`, so a height we only just
    /// started voting on (e.g. at startup / catch-up) is not wrongly declared
    /// stalled (P1-5 + P1-7 interaction, §13.7).
    first_prepared_at: u32,
}

/// How many authority snapshots to keep after their set rotated out.
///
/// Votes are bound to `(set_id, snapshot_root)`, and the runtime can only build
/// the *current* set (session keys move on at rotation). Without a cache every
/// certificate that was still in flight when the session rotated is discarded —
/// on chains with short sessions that means SFF almost never completes a round
/// and fast confirmation trails GRANDPA instead of leading it. The cache is
/// bounded, so a long rotation history cannot grow it.
const MAX_CACHED_SETS: usize = 8;

/// Bounded `set_id -> snapshot` cache, evicting the oldest entry.
#[derive(Default)]
pub struct SetCache {
    sets: HashMap<u64, FinalityAuthoritySet>,
    order: VecDeque<u64>,
}

impl SetCache {
    pub fn get(&self, set_id: u64) -> Option<FinalityAuthoritySet> {
        self.sets.get(&set_id).cloned()
    }

    pub fn insert(&mut self, set: FinalityAuthoritySet) {
        let set_id = set.set_id;
        if self.sets.insert(set_id, set).is_none() {
            self.order.push_back(set_id);
            while self.order.len() > MAX_CACHED_SETS {
                if let Some(oldest) = self.order.pop_front() {
                    self.sets.remove(&oldest);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.sets.len()
    }
}

/// `AuthorityProvider` backed by the `FastFinalityApi` runtime API.
pub struct RuntimeAuthorityProvider<B, C> {
    pub client: Arc<C>,
    /// DEV-ONLY: `(bls_public_key -> proof-of-possession)` for the dev validators.
    ///
    /// The dev chain has no on-chain `register_bls_key` yet (that extrinsic needs
    /// an `EthereumSignature` signer), so the runtime API serves the session key
    /// with an empty PoP. `verify_vote` rightly rejects a vote from an authority
    /// with an invalid PoP, which would stop all voting. For dev we therefore
    /// supply the real PoP here, derived from the same dev seed that produced the
    /// key. Production leaves this empty: the PoP then comes from the on-chain
    /// registration and is verified as-is.
    dev_pops: Vec<(BlsPublicKey, BlsSignature)>,
    /// Snapshots kept for sets that already rotated out, so certificates that
    /// were still in flight at rotation can still be completed and validated.
    cache: Mutex<SetCache>,
    _phantom: PhantomData<B>,
}

impl<B, C> RuntimeAuthorityProvider<B, C> {
    pub fn new(client: Arc<C>, dev_pops: Vec<(BlsPublicKey, BlsSignature)>) -> Self {
        Self {
            client,
            dev_pops,
            cache: Mutex::new(SetCache::default()),
            _phantom: PhantomData,
        }
    }

    fn with_dev_pops(&self, mut set: FinalityAuthoritySet) -> FinalityAuthoritySet {
        if !self.dev_pops.is_empty() {
            for authority in &mut set.authorities {
                if !pop_verify(&authority.bls_public_key, &authority.proof_of_possession) {
                    let found = self
                        .dev_pops
                        .iter()
                        .find(|(pk, _)| *pk == authority.bls_public_key);
                    if let Some((_, pop)) = found {
                        authority.proof_of_possession = *pop;
                    }
                }
            }
        }
        set
    }
}

impl<B, C> AuthorityProvider for RuntimeAuthorityProvider<B, C>
where
    B: BlockT<Hash = H256>,
    C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
    C::Api: FastFinalityApi<B>,
{
    fn current_set_id(&self) -> Option<u64> {
        // Always live: `fast_finality_current_set_id` is a single cheap storage
        // read, and serving a stale id (even briefly after a session rotation)
        // would make a validator sign or verify a vote against the wrong set,
        // breaking quorum. The expensive part — building the authority *set*
        // vector — is cached per `set_id` in `authorities` below, so the
        // per-height healing loop stays cheap without caching this id.
        let at = self.client.info().best_hash;
        self.client
            .runtime_api()
            .fast_finality_current_set_id(at)
            .ok()
    }

    fn authorities(&self, set_id: u64) -> Option<FinalityAuthoritySet> {
        // A captured snapshot is authoritative for a set id until that set
        // rotates out, so the cache serves *both* the active set and rotated-out
        // sets. Consult it first: previously the active set bypassed the cache
        // on every call, forcing `current_set_id` + `fast_finality_authorities`
        // runtime round-trips for every height in `advance_rounds` /
        // `maintain_votes` — ~256 blocking WASM calls per healing tick that
        // stalled the 1s loop for 10-20s and froze fast confirmation.
        if let Some(set) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(set_id)
        {
            return Some(set);
        }
        if self.current_set_id() == Some(set_id) {
            let at = self.client.info().best_hash;
            let set = self
                .client
                .runtime_api()
                .fast_finality_authorities(at, set_id)
                .ok()
                .flatten()?;
            let set = self.with_dev_pops(set);
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(set.clone());
            return Some(set);
        }
        None
    }
}

/// SFF signer backed by a Substrate keystore implementation. Key lookup and
/// signing stay behind the keystore interface; private seed bytes never enter
/// the engine.
pub struct KeystoreSffSigner {
    keystore: KeystorePtr,
}

impl KeystoreSffSigner {
    pub fn new(keystore: KeystorePtr) -> Self {
        Self { keystore }
    }
}

impl SffSigner for KeystoreSffSigner {
    fn public_keys(&self) -> Vec<BlsPublicKey> {
        self.keystore
            .keys(sp_core::crypto::KeyTypeId(*b"sffb"))
            .unwrap_or_else(|error| {
                log::warn!(target: "sff", "failed to enumerate SFF keystore keys: {error}");
                Vec::new()
            })
            .into_iter()
            .filter_map(|raw| BlsPublicKey::from_bytes(&raw))
            .collect()
    }

    fn sign(&self, public: &BlsPublicKey, payload: &[u8]) -> Option<BlsSignature> {
        match self
            .keystore
            .sff_sign(sp_core::crypto::KeyTypeId(*b"sffb"), &public.0, payload)
        {
            Ok(Some(signature)) => BlsSignature::from_bytes(&signature),
            Ok(None) => None,
            Err(error) => {
                log::warn!(target: "sff", "SFF keystore signing failed: {error}");
                None
            }
        }
    }
}

/// The shadow engine.
struct FastFinalityEngine<B, C, P, S: ?Sized> {
    provider: Arc<P>,
    signer: Arc<S>,
    pool: Arc<Mutex<FastFinalityPool>>,
    client: Arc<C>,
    outbound: futures::channel::mpsc::UnboundedSender<Vec<u8>>,
    genesis_hash: H256,
    /// F3: the fast-confirmed view this engine feeds (shared with the RPC).
    view: Arc<FastFinalityView>,
    /// Last time a "dormant" reason was logged (rate limit).
    last_dormant_log: Mutex<Option<std::time::Instant>>,
    /// PoPs are immutable for an authority snapshot; avoid repeating their BLS
    /// pairing checks every time the engine reads the current set.
    checked_pop_sets: Mutex<HashSet<(u64, H256)>>,
    our_index: Mutex<Option<(u64, H256, u32)>>,
    sent_prepares: Mutex<HashSet<(u64, u32, u32)>>,
    sent_commits: Mutex<HashSet<(u64, u32, u32)>>,
    sent_skips: Mutex<HashSet<(u64, u32, u32)>>,
    /// Per-`(set_id, height)` round state for round advancement (P1-5).
    round_state: Mutex<HashMap<(u64, u32), RoundState>>,
    /// Throttle for `RepairRequest`s (P1-6): `(set, height, phase)` -> best height
    /// at which we last asked. Re-ask only after `REPAIR_INTERVAL_BLOCKS`.
    last_repair: Mutex<HashMap<(u64, u32, Phase), u32>>,
    /// Cursor for the catch-up sweep (P1-7): only re-walk the chain when the tip
    /// has advanced, so steady-state does not re-read 64 headers every tick.
    catchup_cursor: Mutex<u32>,
    /// Cursor for QC re-gossip (reliable delivery): only re-push certificates
    /// every `REPAIR_INTERVAL_BLOCKS` so a peer that missed the original broadcast
    /// (transient substream reset, set rotation) still receives it without flooding.
    repush_cursor: Mutex<u32>,
    seen_commit_heights: Mutex<HashSet<(u64, u32)>>,
    /// QCs whose `on_built_qc` reaction has already run (whether built locally or
    /// learned via gossip), so each QC drives our Commit vote / round advance
    /// exactly once (pipeline overlap, §8.2). Pruned by height in `prune_backlog`.
    reacted_qcs: Mutex<HashSet<(u64, Phase, u32, u32)>>,
    _phantom: PhantomData<(B, C)>,
}

impl<B, C, P, S> FastFinalityEngine<B, C, P, S>
where
    B: BlockT<Hash = H256>,
    C: HeaderBackend<B> + Send + Sync + 'static,
    P: AuthorityProvider + 'static,
    S: SffSigner + ?Sized + 'static,
    <<B as BlockT>::Header as HeaderT>::Number: Copy + UniqueSaturatedInto<u32>,
{
    /// Logs `reason` at most once per `DORMANT_LOG_INTERVAL`. A dormant SFF must
    /// be visible: "no certificates" is indistinguishable from "broken" without
    /// this, and silence is the one failure mode operators cannot debug.
    fn note_dormant(&self, reason: &str) {
        let mut last = self
            .last_dormant_log
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if last
            .map(|t| now.duration_since(t) >= DORMANT_LOG_INTERVAL)
            .unwrap_or(true)
        {
            *last = Some(now);
            log::warn!(target: "sff", "SFF is dormant: {reason}");
        }
    }

    /// Returns the current authority snapshot.
    fn current_set(&self) -> Option<FinalityAuthoritySet> {
        let set_id = self.provider.current_set_id()?;
        let set = self.provider.authorities(set_id)?;
        let snapshot = (set.set_id, set.snapshot_root);
        let mut checked = self
            .checked_pop_sets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !checked.contains(&snapshot) {
            // Off-chain PoP re-validation (the runtime cannot do BLS). Log — but
            // keep the set intact so signer indices stay aligned with every vote.
            for a in &set.authorities {
                if !pop_verify(&a.bls_public_key, &a.proof_of_possession) {
                    log::warn!(
                        target: "sff",
                        "authority {} has an invalid BLS proof-of-possession",
                        a.validator_id
                    );
                }
            }
            if checked.len() >= 64 {
                checked.clear();
            }
            checked.insert(snapshot);
        }
        Some(set)
    }

    /// Finds this node's index in the authority set, if it is a registered voter.
    fn ensure_our_index(&self, set: &FinalityAuthoritySet) -> Option<u32> {
        if let Some((cached_set, cached_root, idx)) = *self.our_index.lock().unwrap() {
            if cached_set == set.set_id && cached_root == set.snapshot_root {
                return Some(idx);
            }
        }
        let idx = set
            .authorities
            .iter()
            .position(|a| self.signer.public_keys().contains(&a.bls_public_key))?
            as u32;
        *self.our_index.lock().unwrap() = Some((set.set_id, set.snapshot_root, idx));
        Some(idx)
    }

    /// Called on every successfully imported (and executed) block.
    fn on_block_imported(&self, header: &B::Header, is_new_best: bool) {
        // Side-fork imports are executable but are not a safe local voting target.
        // A later reorg can otherwise make us sign both blocks at one height.
        if !is_new_best {
            return;
        }
        let set = match self.current_set() {
            Some(s) => s,
            None => {
                self.note_dormant("no SFF authority set available from the runtime");
                return;
            }
        };
        let height: u32 = (*header.number()).unique_saturated_into();
        let block_hash = header.hash();
        let parent_hash = *header.parent_hash();
        #[cfg(debug_assertions)]
        {
            let best: u32 = self.client.info().best_number.unique_saturated_into();
            log::debug!(
                target: "sff",
                "on_block_imported height={} best={} lag={}",
                height, best, best.saturating_sub(height)
            );
        }

        // Record/refresh this height's round state; the first sighting starts at
        // round 0, later sightings (reorg re-import) keep the advanced round and
        // update the target block (P1-5, §13.3).
        let round = {
            let mut rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            let st = rs
                .entry((set.set_id, height))
                .or_insert_with(RoundState::default);
            st.block_hash = block_hash;
            st.parent_hash = parent_hash;
            st.current_round
        };

        // Always (re-)prepare the height's current round. The local guard dedups
        // per `(set, height, round)`, so this is a no-op once we have voted.
        self.issue_prepare(&set, height, round, block_hash, parent_hash);
    }

    /// Builds and self-verifies a vote for `(height, round, phase)`. Returns
    /// `None` if this node is not in the set, signing failed, or the signature
    /// does not verify. Does not insert or broadcast — see [`Self::emit_vote`].
    fn build_vote(
        &self,
        set: &FinalityAuthoritySet,
        height: u32,
        round: u32,
        phase: Phase,
        block_hash: Option<H256>,
        parent_hash: H256,
        prepare_qc_hash: Option<H256>,
    ) -> Option<Vote> {
        let idx = self.ensure_our_index(set)?;
        let mut vote = Vote {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash: self.genesis_hash,
            set_id: set.set_id,
            snapshot_root: set.snapshot_root,
            height,
            round,
            phase,
            block_hash,
            parent_hash,
            prepare_qc_hash,
            signer_index: idx,
            signature: BlsSignature::ZERO,
        };
        vote.signature = self.signer.sign(
            &set.authorities[idx as usize].bls_public_key,
            &vote.canonical_bytes(),
        )?;
        if !verify(
            &set.authorities[idx as usize].bls_public_key,
            &vote.canonical_bytes(),
            &vote.signature,
        ) {
            log::warn!(
                target: "sff",
                "self {:?} vote failed verification at height {} round {}; skipping",
                phase, height, round
            );
            return None;
        }
        Some(vote)
    }

    /// Inserts, broadcasts and immediately aggregates a vote.
    fn emit_vote(
        &self,
        set: &FinalityAuthoritySet,
        vote: Vote,
        phase: Phase,
        height: u32,
        round: u32,
    ) {
        self.pool.lock().unwrap().add_vote(vote.clone());
        self.broadcast(SffMessage::Vote(vote));
        self.aggregation_step_for(set, phase, height, round);
    }

    /// Signs and broadcasts a Prepare vote for `(height, round)`, gated by the
    /// per-`(set, height, round)` anti-equivocation guard.
    fn issue_prepare(
        &self,
        set: &FinalityAuthoritySet,
        height: u32,
        round: u32,
        block_hash: H256,
        parent_hash: H256,
    ) {
        if !self
            .sent_prepares
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((set.set_id, height, round))
        {
            return;
        }
        let Some(vote) = self.build_vote(
            set,
            height,
            round,
            Phase::Prepare,
            Some(block_hash),
            parent_hash,
            None,
        ) else {
            self.sent_prepares
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(set.set_id, height, round));
            return;
        };
        // Anchor the round-advancement timeout to when we actually started voting
        // on this (height, round) — not to `best - height` — so a height prepared at
        // startup / catch-up is not spuriously declared stalled, and a round we just
        // advanced to (after a Skip) is given a fresh window instead of inheriting
        // the old round's deadline (which would cascade Skips).
        {
            let best: u32 = self.client.info().best_number.unique_saturated_into();
            let mut rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(st) = rs.get_mut(&(set.set_id, height)) {
                st.first_prepared_at = best;
            }
        }
        log::info!(
            target: "sff",
            "signed Prepare vote set_id={} height={} round={} signer_index={}",
            set.set_id, height, round, vote.signer_index
        );
        self.emit_vote(set, vote, Phase::Prepare, height, round);
    }

    /// Signs and broadcasts a `Skip` vote for `(height, round)` — the signal that
    /// this node is abandoning `round` because its PrepareQC did not form in
    /// time (P1-5). A `SkipQC` (≥ 2/3) lets every node advance to `round + 1`.
    fn issue_skip(&self, set: &FinalityAuthoritySet, height: u32, round: u32) {
        if !self
            .sent_skips
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((set.set_id, height, round))
        {
            return;
        }
        // A Skip attests no block: `block_hash` is `None`, `parent_hash` is zero
        // (both nodes agree), so any two Skip votes for `(height, round)` aggregate.
        let Some(vote) = self.build_vote(set, height, round, Phase::Skip, None, H256::zero(), None)
        else {
            self.sent_skips
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(set.set_id, height, round));
            return;
        };
        log::info!(
            target: "sff",
            "signed Skip vote set_id={} height={} round={} (advancing) signer_index={}",
            set.set_id, height, round, vote.signer_index
        );
        self.emit_vote(set, vote, Phase::Skip, height, round);
    }

    /// Reacts to a built certificate: a PrepareQC triggers our Commit, a SkipQC
    /// advances the local round.
    fn on_built_qc(&self, set: &FinalityAuthoritySet, qc: &sff_primitives::QuorumCertificate) {
        // React to each QC exactly once, whether it was built locally or learned
        // via gossip (pipeline overlap, §8.2). The dedup key is the QC identity.
        let key = (qc.set_id, qc.phase, qc.height, qc.round);
        if !self
            .reacted_qcs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
        {
            return;
        }
        match qc.phase {
            Phase::Prepare => {
                // Record that this round's PrepareQC formed: we will commit and
                // must never advance the round past it.
                {
                    let mut rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(st) = rs.get_mut(&(qc.set_id, qc.height)) {
                        if st.prepare_done.map(|r| r < qc.round).unwrap_or(true) {
                            st.prepare_done = Some(qc.round);
                        }
                    }
                }
                self.maybe_commit(set, qc);
            }
            Phase::Skip => self.on_skip_qc(set, qc.height, qc.round),
            Phase::Commit => {}
        }
    }

    /// A `SkipQC` for `(height, round)` means the network has abandoned `round`;
    /// advance our local round (unless we already committed) and re-prepare the
    /// new round so a PrepareQC can form there (P1-5).
    fn on_skip_qc(&self, set: &FinalityAuthoritySet, height: u32, round: u32) {
        let next = round + 1;
        let target = {
            let mut rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            // Unknown height: adopt the advance but we have no target block to
            // re-prepare against, so just record the round and stop.
            let st = rs
                .entry((set.set_id, height))
                .or_insert_with(RoundState::default);
            if st.prepare_done.is_some() {
                return; // already committed at some round; do not advance.
            }
            if next <= st.current_round {
                return;
            }
            st.current_round = next;
            if st.block_hash.is_zero() {
                None
            } else {
                Some((st.block_hash, st.parent_hash))
            }
        };
        if let Some((block_hash, parent_hash)) = target {
            self.issue_prepare(set, height, next, block_hash, parent_hash);
        }
    }

    /// Reacts to certificates that arrived via gossip rather than being built
    /// locally, so a *received* PrepareQC / SkipQC drives our Commit vote / round
    /// advance exactly like a locally built one (pipeline overlap, §8.2). The
    /// engine already calls `on_built_qc` the moment *it* builds a QC; this is the
    /// symmetric backstop for QCs it only learned about through certificate gossip.
    /// `reacted_qcs` guarantees each QC is processed once regardless of path.
    fn react_to_received_qcs(&self) {
        for phase in [Phase::Prepare, Phase::Skip] {
            let entries = self.pool.lock().unwrap_or_else(|e| e.into_inner()).qc_entries(phase);
            for (set_id, height, round, qc) in entries {
                let key = (set_id, phase, height, round);
                if self
                    .reacted_qcs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&key)
                {
                    continue;
                }
                let Some(set) = self.provider.authorities(set_id) else {
                    continue;
                };
                self.on_built_qc(&set, &qc);
            }
        }
    }

    /// Advances rounds for heights whose PrepareQC has not formed within
    /// `ROUND_TIMEOUT_BLOCKS` *since we first prepared them* (not since the block
    /// height itself — that would wrongly flag a freshly imported / caught-up
    /// height whose `best - height` is already large). Each advance emits a `Skip`
    /// for the abandoned round and re-prepares the next one, so a lost vote or
    /// transient fork no longer stalls a height forever (P1-5, §13.2(d)).
    fn advance_rounds(&self) {
        let best: u32 = self.client.info().best_number.unique_saturated_into();
        // Collect the heights that need advancing (read lock), then act outside it.
        let mut stalled: Vec<(u64, u32, u32, H256, H256)> = Vec::new();
        {
            let rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            for ((set_id, height), st) in rs.iter() {
                if st.prepare_done.is_some() {
                    continue; // committed; nothing to advance.
                }
                // A height we never actually prepared (block unknown, e.g. learned
                // only via a SkipQC) has nothing to re-prepare and must not advance.
                if st.first_prepared_at == 0 {
                    continue;
                }
                if best.saturating_sub(st.first_prepared_at) <= ROUND_TIMEOUT_BLOCKS {
                    continue; // still young; give the PrepareQC time to form.
                }
                log::debug!(
                    target: "sff",
                    "advance_rounds: STALL height={} set_id={} round={} prepare_done={:?} first_prepared_at={} best={} diff={}",
                    height, set_id, st.current_round, st.prepare_done, st.first_prepared_at, best, best.saturating_sub(st.first_prepared_at)
                );
                stalled.push((
                    *set_id,
                    *height,
                    st.current_round,
                    st.block_hash,
                    st.parent_hash,
                ));
            }
        }
        for (set_id, height, round, block_hash, _parent_hash) in stalled {
            let Some(set) = self.provider.authorities(set_id) else {
                continue;
            };
            // Emit a `Skip` for the round we are abandoning. We do NOT advance the
            // local round here: the local round only moves in `on_skip_qc`, once a
            // ≥ 2/3 `SkipQC` is actually built. Advancing locally would desync the
            // two nodes (one forms the round's PrepareQC and commits, the other
            // jumps ahead) and create a permanent hole. Re-transmission in
            // `maintain_votes` usually heals the lost vote before a `SkipQC` is ever
            // needed, so a round is only skipped when ≥ 2/3 genuinely agree.
            let _ = block_hash;
            self.issue_skip(&set, height, round);
        }
    }

    /// Catch-up (P1-7): ensures every height in the working window carries our own
    /// Prepare vote (and, where a PrepareQC already exists, our Commit vote) even
    /// if its import notification was missed or we were briefly offline. This closes
    /// §13.2(d)'s "permanently missing hole": a height that was *never* voted on is
    /// re-prepared here, and a PrepareQC that arrived only via gossip/repair gets
    /// its Commit here. Idempotent via the per-`(set, height, round)` guards, so it
    /// is a no-op once every height in the window is covered.
    /// Re-transmit our votes for heights we already track, to heal votes lost in
    /// gossip and close §13.2(d)'s "permanently missing hole". First coverage of a
    /// height is the job of `on_block_imported` (it prepares every new-best block);
    /// here we only *re-send* Prepare / Commit for heights present in `round_state`
    /// that still lack a certificate. The peer de-duplicates identical votes, so
    /// re-transmission cannot amplify — it just repairs loss. Throttled to every
    /// `REPAIR_INTERVAL_BLOCKS` so steady-state is a no-op.
    fn maintain_votes(&self) {
        let info = self.client.info();
        let best: u32 = info.best_number.unique_saturated_into();
        let finalized: u32 = info.finalized_number.unique_saturated_into();
        // Anchor the healing window at the *frontier* (last fast-confirmed height)
        // rather than at a rolling tail behind best. The only heights whose votes
        // still need gathering are those between the frontier and `best`; anchoring
        // here means a missed vote — even well below the rolling backlog window —
        // is still re-sent, so the frontier advances instead of freezing on a
        // permanent hole. The window `[frontier, best]` is naturally small and
        // shrinks as holes fill, so the per-tick cost stays bounded.
        let frontier = self.view.fast_confirmed_height().unwrap_or(0);
        let floor = finalized.min(frontier);
        {
            let mut cur = self
                .catchup_cursor
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if best.saturating_sub(*cur) < REPAIR_INTERVAL_BLOCKS {
                return;
            }
            *cur = best;
        }
        if floor >= best {
            return;
        }
        // Re-send a bounded batch, starting at the oldest unresolved height.
        // Selecting only the most recent window strands any older hole forever.
        let mut candidates: Vec<(u64, u32, u32, H256, H256)> = {
            let rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut eligible: Vec<_> = rs
                .iter()
                .filter(|((_, h), st)| *h > floor && *h <= best && st.first_prepared_at != 0)
                .map(|((sid, h), st)| (*sid, *h, st.current_round, st.block_hash, st.parent_hash))
                .collect();
            eligible.sort_by_key(|(_, h, _, _, _)| *h);
            eligible
        };
        // Filter completed heights BEFORE applying the batch limit. Otherwise the
        // oldest 32 committed entries permanently occupy every batch and later
        // unresolved heights never get retried.
        {
            let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            candidates.retain(|(sid, h, round, _, _)| {
                pool.qc_for(*sid, Phase::Commit, *h, *round).is_none()
            });
        }
        candidates.truncate(MAX_REPAIR_WINDOW as usize);
        let entries = candidates;
        {
            let mut rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            for (set_id, height, _, _, _) in &entries {
                if let Some(st) = rs.get_mut(&(*set_id, *height)) {
                    // Healing refreshes the staleness clock, preventing a Skip
                    // while redelivery is actively under way.
                    st.first_prepared_at = best;
                }
            }
        }
        log::info!(
            target: "sff",
            "heal: maintain_votes re-sending {} heights [floor={}..best={}]",
            entries.len(), floor, best
        );
        for (set_id, h, round, block_hash, parent_hash) in entries {
            let Some(authorities) = self.provider.authorities(set_id) else {
                continue;
            };
            let (prep_qc, committed) = {
                let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
                let prep = pool.qc_for(set_id, Phase::Prepare, h, round).cloned();
                let committed = pool.qc_for(set_id, Phase::Commit, h, round).is_some();
                (prep, committed)
            };
            if committed {
                continue; // already fast-confirmed at this round; nothing to (re)send.
            }
            // Re-send our Prepare vote for *every* height we prepared but have not
            // yet fast-confirmed. The old guard `prep_qc.is_none()` only re-sent when
            // *we* lacked the PrepareQC — but the peer can be missing *our* Prepare
            // even though *we* already hold the PrepareQC (we saw the peer's Prepare,
            // the peer never saw ours). That case was skipped and left a permanent
            // hole. Re-sending unconditionally is safe: the peer de-duplicates
            // identical votes, so this heals loss without amplification. A genuinely
            // missing Prepare that neither side holds is still recovered because the
            // other node re-sends its own Prepare under the same rule.
            if !block_hash.is_zero() {
                self.resend_vote(
                    &authorities,
                    h,
                    round,
                    Phase::Prepare,
                    Some(block_hash),
                    parent_hash,
                    None,
                );
            }
            // Re-send our Commit vote if we have a PrepareQC but no CommitQC: our
            // Commit may have been lost. (If we already hold the CommitQC we skip —
            // the lagging peer asks for it via `request_repairs` instead of us
            // spraying it.)
            if let Some(prep) = prep_qc {
                self.resend_vote(
                    &authorities,
                    h,
                    round,
                    Phase::Commit,
                    prep.block_hash,
                    prep.parent_hash,
                    Some(prep.vote_payload_hash),
                );
            }
        }
    }

    /// Re-emits one of our votes *without* the per-`(set, height, round)` guard, so
    /// it can re-transmit a vote whose earlier send was lost in gossip. The peer
    /// de-duplicates identical votes, so this heals loss without amplification.
    fn resend_vote(
        &self,
        set: &FinalityAuthoritySet,
        height: u32,
        round: u32,
        phase: Phase,
        block_hash: Option<H256>,
        parent_hash: H256,
        prepare_qc_hash: Option<H256>,
    ) {
        let Some(vote) = self.build_vote(
            set,
            height,
            round,
            phase,
            block_hash,
            parent_hash,
            prepare_qc_hash,
        ) else {
            return;
        };
        self.emit_vote(set, vote, phase, height, round);
    }

    /// Active repair (P1-6): for any height in the window that still lacks a
    /// certificate, ask peers for it. The P2P layer answers a `RepairRequest` by
    /// sending back the matching `Certificate` (point-to-point, never relayed), so a
    /// returned CommitQC reaches the observer via `add_qc` → `new_commit_qcs`, and a
    /// returned PrepareQC is turned into our Commit by `catch_up`. The request uses
    /// each height's *original* `set_id` (from `round_state`), not the current one —
    /// a height's certificate is stored under the set active when it was processed,
    /// and a session rotation would otherwise make the lookup miss. Throttled per
    /// `(set, height, phase)`.
    fn request_repairs(&self) {
        let info = self.client.info();
        let best: u32 = info.best_number.unique_saturated_into();
        let finalized: u32 = info.finalized_number.unique_saturated_into();
        // Anchor at the frontier (last fast-confirmed height) so repair requests
        // cover *every* height above the frontier that still lacks a certificate,
        // not just those inside the rolling backlog window. A missed certificate
        // below the backlog window would otherwise never be requested and would
        // freeze the frontier.
        let frontier = self.view.fast_confirmed_height().unwrap_or(0);
        let floor = finalized.min(frontier);
        if floor >= best {
            return;
        }
        // Iterate `round_state` entries (each carries its original `set_id`) rather
        // than `current_set()`, so the repair request matches how the certificate
        // is keyed.
        let entries: Vec<(u64, u32, u32)> = {
            let rs = self.round_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut eligible: Vec<_> = rs
                .iter()
                .filter(|((_, h), st)| *h > floor && *h <= best && st.first_prepared_at != 0)
                .map(|((sid, h), st)| (*sid, *h, st.current_round))
                .collect();
            eligible.sort_by_key(|(_, h, _)| *h);
            eligible
        };
        let mut to_ask: Vec<RepairRequest> = Vec::new();
        {
            let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let mut last = self.last_repair.lock().unwrap_or_else(|e| e.into_inner());
            for (set_id, h, round) in entries {
                if pool.qc_for(set_id, Phase::Commit, h, round).is_some() {
                    continue; // already fast-confirmed; nothing to repair.
                }
                let (phase, r) = if pool.qc_for(set_id, Phase::Prepare, h, round).is_some() {
                    (Phase::Commit, round)
                } else {
                    (Phase::Prepare, round)
                };
                let key = (set_id, h, phase);
                let prev = last.get(&key).copied().unwrap_or(0);
                if best.saturating_sub(prev) < REPAIR_REQUEST_INTERVAL_BLOCKS {
                    continue;
                }
                to_ask.push(RepairRequest {
                    set_id,
                    height: h,
                    round: r,
                    phase,
                    request_id: best,
                });
                if to_ask.len() >= MAX_REPAIR_WINDOW as usize {
                    break;
                }
            }
            for req in &to_ask {
                last.insert((req.set_id, req.height, req.phase), best);
            }
        }
        for req in to_ask {
            log::debug!(
                target: "sff",
                "repair request set_id={} height={} round={} phase={:?}",
                req.set_id, req.height, req.round, req.phase
            );
            self.broadcast(SffMessage::Repair(req));
        }
    }

    /// Reliable delivery (push-based healing): re-gossip every certificate we hold
    /// for a height still in the working window. The peer de-duplicates identical
    /// certificates by `vote_payload_hash`, so this is pure healing — a peer that
    /// missed the original broadcast (transient substream reset, `set_id` rotation,
    /// brief partition) receives the QC here without any amplification. This is what
    /// actually closes the asymmetric "A has the CommitQC, B is missing it" holes
    /// the pull-based `request_repairs` alone was not reliably clearing: a lost
    /// point-to-point repair answer is recovered by the next re-push. Throttled to
    /// every `REPAIR_INTERVAL_BLOCKS` so steady-state traffic is bounded.
    fn repush_certificates(&self) {
        let info = self.client.info();
        let best: u32 = info.best_number.unique_saturated_into();
        let finalized: u32 = info.finalized_number.unique_saturated_into();
        let frontier = self.view.fast_confirmed_height().unwrap_or(0);
        let floor = finalized.min(frontier);
        if floor >= best {
            return;
        }
        {
            let mut cur = self.repush_cursor.lock().unwrap_or_else(|e| e.into_inner());
            if best.saturating_sub(*cur) < REPUSH_INTERVAL_BLOCKS {
                return;
            }
            *cur = best;
        }
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        // PrepareQCs let a lagging peer that is missing a Prepare vote still form
        // its own Commit; CommitQCs let it fast-confirm directly.
        for (_set_id, h, _round, qc) in pool.qc_entries(Phase::Prepare) {
            if h > floor {
                self.broadcast(SffMessage::Certificate(qc));
            }
        }
        for (_set_id, h, qc) in pool.commit_qc_entries() {
            if h > floor {
                self.broadcast(SffMessage::Certificate(qc));
            }
        }
    }

    /// Aggregate any ready votes for one `(phase, height, round)` and broadcast
    /// the resulting certificate. On a PrepareQC, trigger our Commit vote.
    fn aggregation_step_for(
        &self,
        set: &FinalityAuthoritySet,
        phase: Phase,
        height: u32,
        round: u32,
    ) {
        let qc = {
            self.pool
                .lock()
                .unwrap()
                .try_build_qc(set, phase, height, round)
        };
        if let Some(qc) = qc {
            // Register the built QC in the pool BEFORE broadcasting. The pool's
            // `add_qc` is what feeds `new_commit_qcs`, which the observer drains to
            // publish the F3 fast-confirmation frontier. Previously a locally built
            // QC was only `broadcast`, relying on gossip to echo it back into our own
            // pool — but our own certificate is generally *not* relayed back to us,
            // so the observer never saw it and `fastConfirmedFrontier` stayed `None`
            // on every node even though QCs formed. Self-registering makes the
            // frontier advance from our own QCs regardless of gossip delivery.
            self.pool.lock().unwrap().add_qc(qc.clone());
            log::info!(
                target: "sff",
                "built {}QC set_id={} height={} round={} block={:?}",
                match phase {
                    Phase::Prepare => "Prepare",
                    Phase::Commit => "Commit",
                    _ => "Skip",
                },
                qc.set_id, qc.height, qc.round, qc.block_hash
            );
            self.broadcast(SffMessage::Certificate(qc.clone()));
            self.on_built_qc(set, &qc);
        }
    }

    /// Sweep all pending votes, form any certificates, drive Commit votes, then
    /// run the observer over newly observed CommitQCs.
    fn aggregation_step(&self) {
        // Drive our Commit vote / round advance for any Prepare/Skip QC learned via
        // gossip since the last sweep (pipeline overlap, §8.2). Locally built QCs
        // already called `on_built_qc` directly; `reacted_qcs` dedups the two paths.
        self.react_to_received_qcs();
        // Only keys whose vote set actually changed since the last attempt.
        // Aggregating every pending height on every tick costs one BLS
        // verification per vote, so its cost grows with the backlog — which
        // makes the confirmation frontier fall further behind the faster it
        // needs to run (F3 latency analysis, §13.2(b)).
        let keys = self.pool.lock().unwrap().take_dirty();
        if !keys.is_empty() {
            let mut set_ids: Vec<u64> = keys.iter().map(|(set_id, _, _, _)| *set_id).collect();
            set_ids.sort_unstable();
            set_ids.dedup();
            for set_id in set_ids {
                let Some(set) = self.provider.authorities(set_id) else {
                    continue;
                };
                for (s, phase, height, round) in keys.iter().copied() {
                    if s != set_id {
                        continue;
                    }
                    let qc = self
                        .pool
                        .lock()
                        .unwrap()
                        .try_build_qc(&set, phase, height, round);
                    if let Some(qc) = qc {
                        // Self-register so the observer publishes the frontier from
                        // our own QCs (see `aggregation_step_for` for the rationale).
                        self.pool.lock().unwrap().add_qc(qc.clone());
                        log::info!(
                            target: "sff",
                            "built {}QC set_id={} height={} round={} block={:?}",
                            match qc.phase {
                                sff_primitives::Phase::Prepare => "Prepare",
                                sff_primitives::Phase::Commit => "Commit",
                                sff_primitives::Phase::Skip => "Skip",
                            },
                            qc.set_id, qc.height, qc.round, qc.block_hash
                        );
                        self.broadcast(SffMessage::Certificate(qc.clone()));
                        self.on_built_qc(&set, &qc);
                    }
                }
            }
        }
    }

    /// Healing / catch-up / repair round (P1-5/P1-6/P1-7): re-send our own votes
    /// for any unconfirmed height, pull missing certificates from peers, re-gossip
    /// certificates, advance rounds, prune the backlog, and run the observer.
    ///
    /// This MUST run on a *reliable* cadence, so it is driven by its own 1s task
    /// in `run` rather than the gossip `notify` branch of the main select! loop.
    /// If it ran only on `notify`, a lossy/gappy vote stream would starve the very
    /// re-sends/repairs it needs to recover — which is exactly what made the
    /// confirmation frontier oscillate and lag GRANDPA by many blocks.
    fn healing_step(&self) {
        self.advance_rounds();
        self.maintain_votes();
        self.request_repairs();
        self.repush_certificates();
        self.prune_backlog();
        self.observe();
    }

    /// If a valid PrepareQC exists for `(height, round)` and we have not yet sent
    /// a Commit vote, sign and broadcast the Commit.
    fn maybe_commit(
        &self,
        set: &FinalityAuthoritySet,
        prepare_qc: &sff_primitives::QuorumCertificate,
    ) {
        let height = prepare_qc.height;
        let round = prepare_qc.round;
        if !self
            .sent_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((set.set_id, height, round))
        {
            return;
        }
        let Some(vote) = self.build_vote(
            set,
            height,
            round,
            Phase::Commit,
            prepare_qc.block_hash,
            prepare_qc.parent_hash,
            Some(prepare_qc.vote_payload_hash),
        ) else {
            self.sent_commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(set.set_id, height, round));
            return;
        };
        log::info!(
            target: "sff",
            "signed Commit vote set_id={} height={} round={}",
            set.set_id, height, round
        );
        self.emit_vote(set, vote, Phase::Commit, height, round);
    }

    fn broadcast(&self, msg: SffMessage) {
        let _ = self.outbound.unbounded_send(msg.encode());
    }

    /// Drops anything that can no longer usefully complete.
    ///
    /// A shadow confirmation for a block already finalized by GRANDPA is worth
    /// nothing, and keeping un-completable heights in the pool is what turned the
    /// aggregation cost into O(backlog) (§13.2(b)). So the floor is the GRANDPA
    /// finalized height and the fast-confirmation frontier. Heights above the
    /// frontier may still contain the evidence needed to complete its next step.
    fn prune_backlog(&self) {
        let info = self.client.info();
        let finalized: u32 = info.finalized_number.unique_saturated_into();
        // Anchor pruning at the frontier: votes above it may be exactly what is
        // needed to advance. A fixed `best - N` floor can make an unresolved hole
        // permanent by deleting its missing evidence.
        let frontier = self.view.fast_confirmed_height().unwrap_or(0);
        // Keep unresolved votes above the contiguous frontier: pruning one of
        // these heights makes its missing evidence impossible to recover.
        let floor = finalized.min(frontier);
        let mut pool = self.pool.lock().unwrap();
        pool.set_floor(floor);
        pool.prune_below(floor);
        // Drop per-height bookkeeping for heights that can no longer complete.
        let f = floor;
        self.round_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, h), _| *h >= f);
        self.reacted_qcs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, _, h, _)| *h >= f);
        self.sent_prepares
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, h, _)| *h >= f);
        self.sent_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, h, _)| *h >= f);
        self.sent_skips
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, h, _)| *h >= f);
    }

    /// Shadow observer: hand every newly observed CommitQC to the F3 view, which
    /// classifies it against the GRANDPA finalized chain and decides whether it
    /// may be published as a fast confirmation. Never affects finality.
    fn observe(&self) {
        // Drained, not scanned: the observer runs on every aggregation and must
        // not walk the whole certificate map each time.
        let new: Vec<sff_primitives::QuorumCertificate> = {
            let mut pool = self.pool.lock().unwrap();
            let mut seen = self.seen_commit_heights.lock().unwrap();
            pool.take_new_commit_qcs()
                .into_iter()
                .filter(|qc| seen.insert((qc.set_id, qc.height)))
                .collect()
        };
        if new.is_empty() {
            return;
        }
        let chain = ClientChainView::<C, B>::new(self.client.clone());
        for qc in new {
            self.compare_to_grandpa(&qc, &chain);
        }
    }

    fn compare_to_grandpa(&self, qc: &sff_primitives::QuorumCertificate, chain: &dyn ChainView) {
        let consistency = self.view.observe_commit_qc(qc.clone(), chain);
        match consistency {
            Consistency::OnFinalizedChain => log::debug!(
                target: "sff",
                "CommitQC height {} block {:?} is already GRANDPA finalized",
                qc.height,
                qc.block_hash
            ),
            // Normal fast confirmation: SFF is ahead of GRANDPA, which is exactly
            // what F3 exists to expose. Not a warning.
            Consistency::AheadOfFinality => log::debug!(
                target: "sff",
                "CommitQC height {} block {:?} is ahead of GRANDPA finalized #{} — fast confirmed \
                 (revocable)",
                qc.height,
                qc.block_hash,
                chain.finalized_number()
            ),
            // Divergence: the view stops serving confirmations.
            Consistency::Conflicting => log::warn!(
                target: "sff",
                "SFF CommitQC height {} block {:?} conflicts with the GRANDPA finalized head #{} \
                 ({:?}) — fast confirmations suspended",
                qc.height,
                qc.block_hash,
                chain.finalized_number(),
                chain.finalized_hash()
            ),
            Consistency::Unknown => log::debug!(
                target: "sff",
                "CommitQC height {} block {:?} is not imported locally yet",
                qc.height,
                qc.block_hash
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sff_crypto::{sign, BlsKeypair};

    /// Minimal in-test `SffSigner`: signs with a seed-derived keypair and refuses
    /// any other public key.
    struct DevSeedSigner {
        seed: [u8; 32],
    }

    impl DevSeedSigner {
        fn new(seed: [u8; 32]) -> Self {
            Self { seed }
        }
        fn public(&self) -> BlsPublicKey {
            BlsKeypair::from_seed(&self.seed).public()
        }
    }

    impl SffSigner for DevSeedSigner {
        fn public_keys(&self) -> Vec<BlsPublicKey> {
            vec![self.public()]
        }
        fn sign(&self, public: &BlsPublicKey, payload: &[u8]) -> Option<BlsSignature> {
            let kp = BlsKeypair::from_seed(&self.seed);
            (kp.public() == *public).then(|| sign(&kp, payload))
        }
    }

    #[test]
    fn signer_only_signs_for_its_own_public_key() {
        let signer = DevSeedSigner::new([7; 32]);
        let public = signer.public();
        let payload = b"sff signer interface";
        let signature = signer.sign(&public, payload).expect("matching key signs");
        assert!(sff_crypto::verify(&public, payload, &signature));
        assert!(signer
            .sign(&BlsKeypair::from_seed(&[8; 32]).public(), payload)
            .is_none());
    }

    fn test_set(set_id: u64) -> FinalityAuthoritySet {
        FinalityAuthoritySet::new(set_id, 0, Vec::new()).expect("empty set is valid")
    }

    #[test]
    fn set_cache_keeps_rotated_out_snapshots_within_its_bound() {
        let mut cache = SetCache::default();
        for set_id in 0..(MAX_CACHED_SETS as u64 + 4) {
            cache.insert(test_set(set_id));
        }
        assert_eq!(cache.len(), MAX_CACHED_SETS);
        // The oldest sets were evicted, the most recent ones are still served —
        // which is what lets an in-flight certificate finish after a rotation.
        assert!(cache.get(0).is_none());
        assert!(cache.get(MAX_CACHED_SETS as u64 + 3).is_some());
    }

    #[test]
    fn set_cache_is_idempotent_per_set_id() {
        let mut cache = SetCache::default();
        cache.insert(test_set(7));
        cache.insert(test_set(7));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(7).map(|s| s.set_id), Some(7));
    }

    #[test]
    fn round_state_default_starts_at_zero() {
        let st = RoundState::default();
        assert_eq!(st.current_round, 0);
        assert!(st.prepare_done.is_none());
        assert!(st.block_hash.is_zero());
    }
}

/// Orchestrates the SFF shadow engine (GRANDPA-finality genesis profile): drives
/// voting + aggregation off the block import stream and a periodic tick, and runs
/// the observer. Spawned as a non-essential task so any failure here can never
/// stop BABE/GRANDPA. When SFF becomes the genesis-selected finality axis
/// (strategy doc Stage 3), this task must become essential and feed finality.
pub async fn run_fast_finality<B, C, P, S>(
    provider: Arc<P>,
    signer: Arc<S>,
    pool: Arc<Mutex<FastFinalityPool>>,
    client: Arc<C>,
    outbound: futures::channel::mpsc::UnboundedSender<Vec<u8>>,
    genesis_hash: H256,
    view: Arc<FastFinalityView>,
    mut import_stream: impl Stream<Item = BlockImportNotification<B>> + Unpin + Send + 'static,
) where
    B: BlockT<Hash = H256>,
    C: HeaderBackend<B> + Send + Sync + 'static,
    P: AuthorityProvider + 'static,
    S: SffSigner + ?Sized + 'static,
    <<B as BlockT>::Header as HeaderT>::Number: Copy + UniqueSaturatedInto<u32>,
{
    let notify = Arc::new(Notify::new());
    pool.lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_waker(notify.clone());

    let engine = Arc::new(FastFinalityEngine::<B, C, P, S> {
        provider,
        signer,
        pool,
        client,
        outbound,
        genesis_hash,
        view,
        last_dormant_log: Mutex::new(None),
        checked_pop_sets: Mutex::new(HashSet::new()),
        our_index: Mutex::new(None),
        sent_prepares: Mutex::new(HashSet::new()),
        sent_commits: Mutex::new(HashSet::new()),
        sent_skips: Mutex::new(HashSet::new()),
        round_state: Mutex::new(HashMap::new()),
        last_repair: Mutex::new(HashMap::new()),
        catchup_cursor: Mutex::new(0),
        repush_cursor: Mutex::new(0),
        seen_commit_heights: Mutex::new(HashSet::new()),
        reacted_qcs: Mutex::new(HashSet::new()),
        _phantom: PhantomData,
    });

    log::info!(target: "sff", "SFF shadow engine started (genesis {genesis_hash})");

    // Healing (vote re-send / repair / re-gossip / round-advance / prune /
    // observe, P1-5..P1-7) runs on its OWN reliable 1s task, fully decoupled
    // from the gossip `notify`. If it ran on `notify`, a lossy or gappy vote
    // stream would starve the very re-sends and repairs it needs to recover from
    // exactly those losses — which previously made the confirmation frontier
    // oscillate and lag GRANDPA by many blocks. Running it independently keeps
    // re-sends/repairs flowing at a steady 1s cadence no matter how bad the
    // underlying gossip is.
    let heal_engine = engine.clone();
    tokio::spawn(async move {
        let mut timer = interval(Duration::from_millis(1_000));
        loop {
            timer.tick().await;
            heal_engine.healing_step();
        }
    });

    // Vote aggregation (the dirty-key sweep that builds QCs and drives commits)
    // is event-driven: the pool notifies on every inserted vote. `import_stream`
    // is biased-prioritized so block imports are always prepared promptly
    // (preventing frontier lag from import starvation); `notify` aggregates the
    // votes as they arrive; the timer is an idle fallback.
    let mut timer = interval(Duration::from_millis(1_000));
    loop {
        // `biased` polls the branches in source order so a pending block-import
        // notification is always preferred over the (almost always ready) gossip
        // `notify`. Without this, a fair `select!` can starve `import_stream`
        // behind the continuous stream of vote notifications, causing SFF to
        // prepare heights many blocks after they are imported — which makes the
        // fast-confirmation frontier lag GRANDPA by exactly that delay.
        tokio::select! {
            biased;
            notif = import_stream.next() => {
                match notif {
                    Some(n) => engine.on_block_imported(&n.header, n.is_new_best),
                    None => break, // stream ended
                }
            }
            _ = notify.notified() => {
                engine.aggregation_step();
            }
            _ = timer.tick() => {
                engine.aggregation_step();
            }
        }
    }
    log::info!(target: "sff", "shadow engine loop terminated");
}
