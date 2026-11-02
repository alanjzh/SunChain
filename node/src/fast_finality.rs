//! Sunchain Fast Finality (SFF) F2 shadow engine: voter, aggregator, observer
//! and their orchestration (design §5 `fast_finality_voter.rs`,
//! `fast_finality_aggregator.rs`, `fast_finality_observer.rs`).
//!
//! **F2 is strictly shadow.** The engine produces SFF votes + certificates, but
//! they never reach fork choice, runtime state or user finality. The only
//! outward effect is logging / metering of how SFF's fast confirmation compares
//! to GRANDPA's authoritative finality. Any SFF fault (timeout, bad certificate,
//! equivocation, network split) is logged and rate-limited here — it must never
//! interrupt BABE authoring or GRANDPA finality.

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

use sff_crypto::{pop_verify, verify_vote};
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

/// How many blocks behind the tip SFF may still track.
///
/// Anything older is dropped: a shadow confirmation that far behind is useless,
/// and keeping it is what made aggregation cost grow with the backlog.
const MAX_BACKLOG_HEIGHTS: u32 = 64;

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
        let at = self.client.info().best_hash;
        self.client
            .runtime_api()
            .fast_finality_current_set_id(at)
            .ok()
    }

    fn authorities(&self, set_id: u64) -> Option<FinalityAuthoritySet> {
        // The runtime can only build the *current* set, so a rotated-out set is
        // served from the snapshot captured while it was active.
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
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(set_id)
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
    our_index: Mutex<Option<(u64, H256, u32)>>,
    sent_prepares: Mutex<HashSet<(u64, u32, u32)>>,
    sent_commits: Mutex<HashSet<(u64, u32, u32)>>,
    seen_commit_heights: Mutex<HashSet<(u64, u32)>>,
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
        let mut last = self.last_dormant_log.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if last.map(|t| now.duration_since(t) >= DORMANT_LOG_INTERVAL).unwrap_or(true) {
            *last = Some(now);
            log::warn!(target: "sff", "SFF is dormant: {reason}");
        }
    }

    /// Returns the current authority snapshot.
    fn current_set(&self) -> Option<FinalityAuthoritySet> {
        let set_id = self.provider.current_set_id()?;
        let set = self.provider.authorities(set_id)?;
        // Off-chain PoP re-validation (the runtime cannot do BLS). Log — but keep
        // the set intact so signer indices stay aligned with every vote.
        for a in &set.authorities {
            if !pop_verify(&a.bls_public_key, &a.proof_of_possession) {
                log::warn!(
                    target: "sff",
                    "authority {} has an invalid BLS proof-of-possession",
                    a.validator_id
                );
            }
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
        let idx = match self.ensure_our_index(&set) {
            Some(i) => i,
            None => {
                self.note_dormant("this node is not a member of the SFF authority set");
                return;
            }
        };
        let height: u32 = (*header.number()).unique_saturated_into();
        let block_hash = header.hash();
        let parent_hash = *header.parent_hash();

        let mut vote = Vote {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash: self.genesis_hash,
            set_id: set.set_id,
            snapshot_root: set.snapshot_root,
            height,
            round: 0,
            phase: Phase::Prepare,
            block_hash: Some(block_hash),
            parent_hash,
            prepare_qc_hash: None,
            signer_index: idx,
            signature: BlsSignature::ZERO,
        };
        vote.signature = match self.signer.sign(
            &set.authorities[idx as usize].bls_public_key,
            &vote.canonical_bytes(),
        ) {
            Some(s) => s,
            None => return,
        };
        if verify_vote(&vote, &set).is_err() {
            log::warn!(target: "sff", "self Prepare vote failed verification; skipping");
            return;
        }
        // This is the durable local anti-equivocation boundary, independent of
        // pool de-duplication and gossip ordering.
        if !claim_prepare(&self.sent_prepares, set.set_id, height, 0) {
            return;
        }

        log::info!(
            target: "sff",
            "signed Prepare vote set_id={} height={} round=0 signer_index={}",
            set.set_id, height, idx
        );
        self.pool.lock().unwrap().add_vote(vote.clone());
        self.broadcast(SffMessage::Vote(vote));
        self.aggregation_step_for(&set, Phase::Prepare, height, 0);
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
            if phase == Phase::Prepare {
                self.maybe_commit(set, &qc);
            }
        }
    }

    /// Sweep all pending votes, form any certificates, drive Commit votes, then
    /// run the observer over newly observed CommitQCs.
    fn aggregation_step(&self) {
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
                        if phase == Phase::Prepare {
                            self.maybe_commit(&set, &qc);
                        }
                    }
                }
            }
        }
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
            .unwrap()
            .insert((set.set_id, height, round))
        {
            return;
        }
        let idx = match self.ensure_our_index(set) {
            Some(i) => i,
            None => {
                self.sent_commits
                    .lock()
                    .unwrap()
                    .remove(&(set.set_id, height, round));
                return;
            }
        };
        let mut vote = Vote {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash: self.genesis_hash,
            set_id: set.set_id,
            snapshot_root: set.snapshot_root,
            height,
            round,
            phase: Phase::Commit,
            block_hash: prepare_qc.block_hash,
            parent_hash: prepare_qc.parent_hash,
            prepare_qc_hash: Some(prepare_qc.vote_payload_hash),
            signer_index: idx,
            signature: BlsSignature::ZERO,
        };
        vote.signature = match self.signer.sign(
            &set.authorities[idx as usize].bls_public_key,
            &vote.canonical_bytes(),
        ) {
            Some(s) => s,
            None => {
                self.sent_commits
                    .lock()
                    .unwrap()
                    .remove(&(set.set_id, height, round));
                return;
            }
        };
        if verify_vote(&vote, set).is_err() {
            self.sent_commits
                .lock()
                .unwrap()
                .remove(&(set.set_id, height, round));
            return;
        }
        log::info!(
            target: "sff",
            "signed Commit vote set_id={} height={} round={}",
            set.set_id, height, round
        );
        self.pool.lock().unwrap().add_vote(vote.clone());
        self.broadcast(SffMessage::Vote(vote));
        self.aggregation_step_for(set, Phase::Commit, height, round);
    }

    fn broadcast(&self, msg: SffMessage) {
        let _ = self.outbound.unbounded_send(msg.encode());
    }

    /// Drops anything that can no longer usefully complete.
    ///
    /// A shadow confirmation for a block already finalized by GRANDPA is worth
    /// nothing, and keeping un-completable heights in the pool is what turned the
    /// aggregation cost into O(backlog) (§13.2(b)). So the floor is the GRANDPA
    /// finalized height: a height is only ever pruned *after* it is final and
    /// therefore can never still be completing — which is why a fixed tail
    /// (`best - N`) caused holes (a height pruned before its CommitQC formed).
    /// The fixed window is only a safety net if GRANDPA stalls further than
    /// `MAX_BACKLOG_HEIGHTS` behind the tip.
    fn prune_backlog(&self) {
        let info = self.client.info();
        let best: u32 = info.best_number.unique_saturated_into();
        let finalized: u32 = info.finalized_number.unique_saturated_into();
        let floor = finalized.min(best.saturating_sub(MAX_BACKLOG_HEIGHTS));
        let mut pool = self.pool.lock().unwrap();
        pool.set_floor(floor);
        pool.prune_below(floor);
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

fn claim_prepare(
    sent_prepares: &Mutex<HashSet<(u64, u32, u32)>>,
    set_id: u64,
    height: u32,
    round: u32,
) -> bool {
    sent_prepares
        .lock()
        .unwrap()
        .insert((set_id, height, round))
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
    fn prepare_guard_allows_only_one_target_per_set_height_round() {
        let sent = Mutex::new(HashSet::new());
        assert!(claim_prepare(&sent, 4, 20, 0));
        assert!(!claim_prepare(&sent, 4, 20, 0));
        assert!(claim_prepare(&sent, 5, 20, 0));
    }
}

/// Orchestrates the SFF shadow engine: drives voting + aggregation off the block
/// import stream and a periodic tick, and runs the observer. Spawned as a
/// non-essential task so any failure here can never stop BABE/GRANDPA.
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

    let engine = FastFinalityEngine::<B, C, P, S> {
        provider,
        signer,
        pool,
        client,
        outbound,
        genesis_hash,
        view,
        last_dormant_log: Mutex::new(None),
        our_index: Mutex::new(None),
        sent_prepares: Mutex::new(HashSet::new()),
        sent_commits: Mutex::new(HashSet::new()),
        seen_commit_heights: Mutex::new(HashSet::new()),
        _phantom: PhantomData,
    };

    log::info!(target: "sff", "SFF shadow engine started (genesis {genesis_hash})");

    // Aggregation is event-driven (the pool notifies on every insert); the timer
    // is only a safety net for pruning and anything that slipped through.
    let mut timer = interval(Duration::from_millis(1_000));
    loop {
        tokio::select! {
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

/// Builds a repair request for a missing certificate (used by the gossip reply).
#[allow(dead_code)]
pub fn repair_for(phase: Phase, height: u32, round: u32, set_id: u64) -> SffMessage {
    SffMessage::Repair(RepairRequest {
        set_id,
        height,
        round,
        phase,
    })
}
