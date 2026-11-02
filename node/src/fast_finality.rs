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
    collections::{HashMap, HashSet},
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
use tokio::time::interval;

use sff_crypto::{pop_verify, verify_vote};
use sff_primitives::{
    BlsPublicKey, BlsSignature, FinalityAuthoritySet, Phase, Vote, SFF_PROTOCOL_VERSION,
};

use crate::fast_finality_p2p::{RepairRequest, SffMessage};
use crate::fast_finality_pool::FastFinalityPool;

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
    _phantom: PhantomData<B>,
}

impl<B, C> RuntimeAuthorityProvider<B, C> {
    pub fn new(client: Arc<C>, dev_pops: Vec<(BlsPublicKey, BlsSignature)>) -> Self {
        Self {
            client,
            dev_pops,
            _phantom: PhantomData,
        }
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
        let at = self.client.info().best_hash;
        let mut set = self
            .client
            .runtime_api()
            .fast_finality_authorities(at, set_id)
            .ok()
            .flatten()?;
        // Fill in the dev PoPs where the runtime could not serve one (see
        // `dev_pops`). Without a valid PoP no vote verifies, so the shadow path
        // would silently produce nothing.
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
        Some(set)
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
            None => return, // no registered authority set yet
        };
        let idx = match self.ensure_our_index(&set) {
            Some(i) => i,
            None => return, // we are not a registered SFF voter
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
        let set = match self.current_set() {
            Some(s) => s,
            None => return,
        };
        let keys = self.pool.lock().unwrap().votes_keys();
        for (set_id, phase, height, round) in keys {
            if set_id != set.set_id {
                continue;
            }
            let qc = self
                .pool
                .lock()
                .unwrap()
                .try_build_qc(&set, phase, height, round);
            if let Some(qc) = qc {
                self.broadcast(SffMessage::Certificate(qc.clone()));
                if phase == Phase::Prepare {
                    self.maybe_commit(&set, &qc);
                }
            }
        }
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

    /// Shadow observer: for each newly observed CommitQC, compare its block to the
    /// GRANDPA finalized chain. Logs only — never affects finality.
    fn observe(&self) {
        let new: Vec<sff_primitives::QuorumCertificate> = {
            let pool = self.pool.lock().unwrap();
            let mut seen = self.seen_commit_heights.lock().unwrap();
            pool.commit_qc_entries()
                .into_iter()
                .filter(|(set_id, h, _)| seen.insert((*set_id, *h)))
                .map(|(_, _, qc)| qc)
                .collect()
        };
        for qc in new {
            self.compare_to_grandpa(&qc);
        }
    }

    fn compare_to_grandpa(&self, qc: &sff_primitives::QuorumCertificate) {
        let Some(block_hash) = qc.block_hash else {
            return;
        };
        let finalized = self.client.info().finalized_hash;
        // A QC is on the finalized chain iff its block is an ancestor of the
        // current GRANDPA finalized head.  The inverse merely says it descends
        // from finality and may still be reversible.
        let on_chain = self.is_ancestor_of(finalized, block_hash);
        if on_chain {
            log::debug!(
                target: "sff",
                "CommitQC height {} block {:?} is on the GRANDPA canon chain",
                qc.height,
                qc.block_hash
            );
        } else {
            // SFF committed to a block GRANDPA has not finalized (or finalized a
            // different branch) — a shadow diff to watch, not an error.
            log::warn!(
                target: "sff",
                "SFF CommitQC height {} block {:?} is NOT on the GRANDPA finalized chain (finalized {:?})",
                qc.height,
                qc.block_hash,
                finalized
            );
        }
    }

    /// Walks up `candidate`'s ancestry looking for `ancestor`.
    fn is_ancestor_of(&self, mut candidate: H256, ancestor: H256) -> bool {
        if candidate == ancestor {
            return true;
        }
        for _ in 0..256 {
            let hdr = match self.client.header(candidate) {
                Ok(Some(h)) => h,
                _ => return false,
            };
            let parent = *hdr.parent_hash();
            if parent == ancestor {
                return true;
            }
            if parent == candidate {
                return false;
            }
            candidate = parent;
        }
        false
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
    mut import_stream: impl Stream<Item = BlockImportNotification<B>> + Unpin + Send + 'static,
) where
    B: BlockT<Hash = H256>,
    C: HeaderBackend<B> + Send + Sync + 'static,
    P: AuthorityProvider + 'static,
    S: SffSigner + ?Sized + 'static,
    <<B as BlockT>::Header as HeaderT>::Number: Copy + UniqueSaturatedInto<u32>,
{
    let engine = FastFinalityEngine::<B, C, P, S> {
        provider,
        signer,
        pool,
        client,
        outbound,
        genesis_hash,
        our_index: Mutex::new(None),
        sent_prepares: Mutex::new(HashSet::new()),
        sent_commits: Mutex::new(HashSet::new()),
        seen_commit_heights: Mutex::new(HashSet::new()),
        _phantom: PhantomData,
    };

    log::info!(target: "sff", "SFF shadow engine started (genesis {genesis_hash})");

    let mut timer = interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            notif = import_stream.next() => {
                match notif {
                    Some(n) => engine.on_block_imported(&n.header, n.is_new_best),
                    None => break, // stream ended
                }
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
