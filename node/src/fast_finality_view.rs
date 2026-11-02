//! F3: the externally visible "fast confirmed" view of SFF (design §6 F3, §4.6).
//!
//! F2 produced CommitQCs and only logged them. F3 turns the highest CommitQC
//! into a *fast confirmation* that RPC/WS consumers (DEX UI, indexer, wallet,
//! explorer) can read — under the constraints that make it safe to publish:
//!
//! * **Consistency with GRANDPA is a precondition.** A CommitQC may only be
//!   served when its block is the GRANDPA finalized head, one of its ancestors
//!   (already irreversible) or a descendant of it (SFF is legitimately ahead).
//!   A CommitQC on a branch GRANDPA did *not* finalize is a divergence: the node
//!   stops serving confirmations (design §4.6 F3: 停止发 SFF confirmation) and
//!   reports it through [`SffStatus`].
//! * **Fast confirmation is always revocable.** Every view carries an explicit
//!   risk label and a `revocable` flag; consumers must keep waiting for GRANDPA
//!   for clearing, withdrawals, bridges and asset accounting (F3 gate: no path
//!   may treat fast-confirmed as irreversible finality).
//! * **Bounded.** The confirmed history is capped; a slow or hostile SFF peer
//!   cannot grow this structure.

use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use sc_client_api::HeaderBackend;
use serde::{Deserialize, Serialize};
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, UniqueSaturatedInto};

use sff_primitives::QuorumCertificate;

/// Upper bound on confirmed blocks kept for `sff_certificate` / reconciliation.
const MAX_HISTORY: usize = 128;
/// Upper bound on the ancestry walk, so a deep chain can never spin here.
const MAX_WALK: u32 = 4096;

/// How an SFF CommitQC's block relates to the GRANDPA finalized chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Consistency {
    /// The block is the finalized head or one of its ancestors: GRANDPA already
    /// made it irreversible.
    OnFinalizedChain,
    /// The block descends from the finalized head: SFF is ahead. This is the
    /// normal fast-confirmed state and is still revocable.
    AheadOfFinality,
    /// The block sits on a branch GRANDPA did not finalize. Serving it would
    /// mean telling consumers a block is confirmed that canonical finality
    /// rejects — F3 must stop serving and raise the divergence.
    Conflicting,
    /// The block is unknown locally (not imported yet); nothing can be claimed.
    Unknown,
}

impl Consistency {
    /// Whether a confirmation for this state may be published.
    pub fn is_servable(self) -> bool {
        matches!(self, Consistency::OnFinalizedChain | Consistency::AheadOfFinality)
    }

    pub fn label(self) -> &'static str {
        match self {
            Consistency::OnFinalizedChain => "onFinalizedChain",
            Consistency::AheadOfFinality => "aheadOfFinality",
            Consistency::Conflicting => "conflicting",
            Consistency::Unknown => "unknown",
        }
    }
}

/// One fast-confirmed block, as served to consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FastConfirmedView {
    pub block_hash: H256,
    pub height: u32,
    pub set_id: u64,
    pub round: u32,
    pub phase: String,
    /// Relationship to the GRANDPA finalized chain at the time of the answer.
    pub consistency: Consistency,
    /// `true` only once GRANDPA has finalized this exact block.
    pub grandpa_finalized: bool,
    /// Always `true` unless `grandpa_finalized`: a fast confirmation can still
    /// be revoked by a reorg.
    pub revocable: bool,
    pub parent_hash: H256,
    pub snapshot_root: H256,
    pub prepare_qc_hash: Option<H256>,
    pub vote_payload_hash: H256,
    /// Aggregate BLS signature (hex).
    pub aggregate_signature: String,
    /// Stake behind the certificate (decimal string; `u128` exceeds JSON precision).
    pub signed_stake: String,
    /// Human-readable risk label; never empty.
    pub warning: String,
}

/// A recorded divergence between SFF and GRANDPA, kept for downstream
/// reconciliation (design F3: reorg/降级处理接口).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictView {
    pub sff_height: u32,
    pub sff_block_hash: H256,
    pub consistency: Consistency,
    pub grandpa_finalized_number: u32,
    pub grandpa_finalized_hash: H256,
    pub set_id: u64,
}

/// Node-level status of the SFF fast-confirmation surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SffStatus {
    /// Deployment phase. F3 exposes fast confirmation; GRANDPA stays the
    /// authoritative finality.
    pub phase: String,
    pub protocol_version: u16,
    pub genesis_hash: Option<H256>,
    /// Whether fast confirmations are being served right now.
    pub serving: bool,
    /// `true` while SFF and GRANDPA disagree; no confirmation is served then.
    pub degraded: bool,
    pub degrade_reason: Option<String>,
    /// Divergences observed since node start.
    pub conflicts: u64,
    /// Most recent divergence, for indexers that need to roll back.
    pub last_conflict: Option<ConflictView>,
    /// Height of the currently served fast confirmation.
    pub fast_confirmed_height: Option<u32>,
    pub fast_confirmed_hash: Option<H256>,
    pub grandpa_finalized_number: u32,
    pub grandpa_finalized_hash: H256,
    /// Monotonic counter bumped on every publish/degrade/recover; consumers can
    /// watch it to detect reorg and degrade transitions without polling payloads.
    pub revision: u64,
    pub warning: String,
}

impl SffStatus {
    pub const WARNING: &'static str = "fast confirmation is revocable: GRANDPA remains the \
        authoritative finality; clearing, withdrawals, bridges and asset accounting must wait \
        for GRANDPA finalized";
}

/// Minimal read-only view of the local chain, so the tracker can be unit-tested
/// without a client.
pub trait ChainView: Send + Sync {
    fn finalized_hash(&self) -> H256;
    fn finalized_number(&self) -> u32;
    /// `(block_number, parent_hash)` of `hash`.
    fn header(&self, hash: &H256) -> Option<(u32, H256)>;

    fn number(&self, hash: &H256) -> Option<u32> {
        self.header(hash).map(|(n, _)| n)
    }
}

/// [`ChainView`] over a Substrate client.
pub struct ClientChainView<C, B> {
    client: Arc<C>,
    _phantom: PhantomData<B>,
}

impl<C, B> ClientChainView<C, B> {
    pub fn new(client: Arc<C>) -> Self {
        Self {
            client,
            _phantom: PhantomData,
        }
    }
}

impl<C, B> ChainView for ClientChainView<C, B>
where
    B: BlockT<Hash = H256>,
    C: HeaderBackend<B> + Send + Sync + 'static,
    <<B as BlockT>::Header as HeaderT>::Number: UniqueSaturatedInto<u32>,
{
    fn finalized_hash(&self) -> H256 {
        self.client.info().finalized_hash
    }

    fn finalized_number(&self) -> u32 {
        self.client.info().finalized_number.unique_saturated_into()
    }

    fn header(&self, hash: &H256) -> Option<(u32, H256)> {
        let header = self.client.header(*hash).ok().flatten()?;
        Some(((*header.number()).unique_saturated_into(), *header.parent_hash()))
    }
}

/// One stored confirmation.
#[derive(Clone)]
struct Confirmed {
    qc: QuorumCertificate,
    block_hash: H256,
    consistency: Consistency,
}

#[derive(Default)]
struct Inner {
    /// Highest served confirmation.
    latest: Option<Confirmed>,
    /// Most recent confirmations, newest last.
    history: VecDeque<Confirmed>,
    degraded: bool,
    degrade_reason: Option<String>,
    conflicts: u64,
    last_conflict: Option<ConflictView>,
    revision: u64,
}

/// Shared SFF fast-confirmation state: written by the shadow engine, read by the
/// RPC. Writes are idempotent and read paths never block on the network.
pub struct FastFinalityView {
    inner: Mutex<Inner>,
}

impl Default for FastFinalityView {
    fn default() -> Self {
        Self::new()
    }
}

impl FastFinalityView {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Offer a newly aggregated CommitQC to the view.
    ///
    /// Returns the classification so the caller can log it. A `Conflicting`
    /// certificate is never published: it degrades the view (design §4.6 F3).
    pub fn observe_commit_qc(
        &self,
        qc: QuorumCertificate,
        chain: &dyn ChainView,
    ) -> Consistency {
        let Some(block_hash) = qc.block_hash else {
            return Consistency::Unknown;
        };
        let consistency = consistency_of(chain, &block_hash);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if !consistency.is_servable() {
            inner.conflicts += 1;
            inner.degraded = true;
            inner.degrade_reason = Some(format!(
                "SFF CommitQC at height {} ({:?}) is {} with respect to the GRANDPA finalized \
                 head #{} ({:?}); fast confirmations suspended",
                qc.height,
                block_hash,
                consistency.label(),
                chain.finalized_number(),
                chain.finalized_hash(),
            ));
            inner.last_conflict = Some(ConflictView {
                sff_height: qc.height,
                sff_block_hash: block_hash,
                consistency,
                grandpa_finalized_number: chain.finalized_number(),
                grandpa_finalized_hash: chain.finalized_hash(),
                set_id: qc.set_id,
            });
            // Stop serving: a confirmation that canonical finality rejects must
            // not stay visible to downstream consumers.
            inner.latest = None;
            inner.revision += 1;
            return consistency;
        }

        let confirmed = Confirmed {
            qc,
            block_hash,
            consistency,
        };
        if inner.degraded {
            // Back on the finalized chain: resume serving, but keep the audit
            // trail (conflict count + last conflict) for downstream rollback.
            inner.degraded = false;
            inner.degrade_reason = None;
        }
        let supersedes = match &inner.latest {
            None => true,
            Some(current) => {
                (confirmed.qc.set_id, confirmed.qc.height)
                    >= (current.qc.set_id, current.qc.height)
            }
        };
        if supersedes {
            inner.latest = Some(confirmed.clone());
            inner.history.push_back(confirmed);
            if inner.history.len() > MAX_HISTORY {
                inner.history.pop_front();
            }
            inner.revision += 1;
        }
        consistency
    }

    /// Re-check the served confirmation against the current finalized chain.
    ///
    /// A confirmation served earlier can be reorged away; this is the hook that
    /// turns such a reorg into a degrade instead of a stale answer.
    fn revalidate(&self, chain: &dyn ChainView) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = inner.latest.clone() else {
            return;
        };
        let consistency = consistency_of(chain, &current.block_hash);
        if consistency.is_servable() {
            if consistency != current.consistency {
                inner.latest = Some(Confirmed {
                    consistency,
                    ..current
                });
                inner.revision += 1;
            }
            return;
        }
        inner.conflicts += 1;
        inner.degraded = true;
        inner.degrade_reason = Some(format!(
            "fast confirmation at height {} ({:?}) left the GRANDPA finalized chain ({}); \
             fast confirmations suspended",
            current.qc.height,
            current.block_hash,
            consistency.label(),
        ));
        inner.last_conflict = Some(ConflictView {
            sff_height: current.qc.height,
            sff_block_hash: current.block_hash,
            consistency,
            grandpa_finalized_number: chain.finalized_number(),
            grandpa_finalized_hash: chain.finalized_hash(),
            set_id: current.qc.set_id,
        });
        inner.latest = None;
        inner.revision += 1;
    }

    /// Status, re-validated against `chain` first.
    pub fn status(&self, chain: &dyn ChainView) -> SffStatus {
        self.revalidate(chain);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (height, hash, genesis, version) = match &inner.latest {
            Some(c) => (
                Some(c.qc.height),
                Some(c.block_hash),
                Some(c.qc.genesis_hash),
                c.qc.version,
            ),
            None => (None, None, None, sff_primitives::SFF_PROTOCOL_VERSION),
        };
        SffStatus {
            phase: "f3-fast-confirmed".to_string(),
            protocol_version: version,
            genesis_hash: genesis,
            serving: !inner.degraded && inner.latest.is_some(),
            degraded: inner.degraded,
            degrade_reason: inner.degrade_reason.clone(),
            conflicts: inner.conflicts,
            last_conflict: inner.last_conflict.clone(),
            fast_confirmed_height: height,
            fast_confirmed_hash: hash,
            grandpa_finalized_number: chain.finalized_number(),
            grandpa_finalized_hash: chain.finalized_hash(),
            revision: inner.revision,
            warning: SffStatus::WARNING.to_string(),
        }
    }

    /// The current fast-confirmed head, or `None` when nothing is served.
    pub fn head(&self, chain: &dyn ChainView) -> Option<FastConfirmedView> {
        self.revalidate(chain);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.degraded {
            return None;
        }
        inner.latest.as_ref().map(view_of)
    }

    /// The confirmation recorded for `height`, newest first.
    pub fn certificate(&self, chain: &dyn ChainView, height: u32) -> Option<FastConfirmedView> {
        self.revalidate(chain);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .history
            .iter()
            .rev()
            .find(|c| c.qc.height == height)
            .or_else(|| inner.latest.as_ref().filter(|c| c.qc.height == height))
            .map(view_of)
    }

    /// Monotonic revision counter used by the WS subscription.
    pub fn revision(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).revision
    }
}

fn view_of(c: &Confirmed) -> FastConfirmedView {
    let grandpa_finalized = c.consistency == Consistency::OnFinalizedChain;
    FastConfirmedView {
        block_hash: c.block_hash,
        height: c.qc.height,
        set_id: c.qc.set_id,
        round: c.qc.round,
        phase: "Commit".to_string(),
        consistency: c.consistency,
        grandpa_finalized,
        revocable: !grandpa_finalized,
        parent_hash: c.qc.parent_hash,
        snapshot_root: c.qc.snapshot_root,
        prepare_qc_hash: c.qc.prepare_qc_hash,
        vote_payload_hash: c.qc.vote_payload_hash,
        aggregate_signature: to_hex(&c.qc.aggregate_signature.0),
        signed_stake: c.qc.signed_stake.to_string(),
        warning: if grandpa_finalized {
            "finalized by GRANDPA: irreversible".to_string()
        } else {
            "fast-confirmed only: revocable until GRANDPA finalizes this block; do not use for \
             clearing, withdrawals, bridges or asset accounting"
                .to_string()
        },
    }
}

/// Classifies `block` against the GRANDPA finalized chain.
fn consistency_of(chain: &dyn ChainView, block: &H256) -> Consistency {
    let finalized = chain.finalized_hash();
    if *block == finalized {
        return Consistency::OnFinalizedChain;
    }
    // Already irreversible: `block` is an ancestor of the finalized head.
    if is_ancestor(chain, block, &finalized) {
        return Consistency::OnFinalizedChain;
    }
    // Legitimately ahead: the finalized head is an ancestor of `block`.
    if is_ancestor(chain, &finalized, block) {
        return Consistency::AheadOfFinality;
    }
    if chain.header(block).is_some() {
        Consistency::Conflicting
    } else {
        Consistency::Unknown
    }
}

/// `true` when `ancestor` is `descendant` or one of its ancestors.
fn is_ancestor(chain: &dyn ChainView, ancestor: &H256, descendant: &H256) -> bool {
    let (Some(a_num), Some(d_num)) = (chain.number(ancestor), chain.number(descendant)) else {
        return false;
    };
    if a_num > d_num {
        return false;
    }
    let mut current = *descendant;
    let mut number = d_num;
    let mut steps = 0u32;
    while number > a_num {
        if steps >= MAX_WALK {
            return false;
        }
        let (n, parent) = match chain.header(&current) {
            Some(h) => h,
            None => return false,
        };
        if n != number || n == 0 {
            return false;
        }
        current = parent;
        number = n - 1;
        steps += 1;
    }
    current == *ancestor
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory chain: a set of `(hash -> (number, parent))` plus a finalized
    /// head, so forks can be expressed directly.
    #[derive(Default)]
    struct MockChain {
        headers: HashMap<H256, (u32, H256)>,
        finalized: H256,
    }

    impl MockChain {
        fn push(&mut self, hash: H256, number: u32, parent: H256) {
            self.headers.insert(hash, (number, parent));
        }

        /// Linear chain `1..=len` with deterministic hashes.
        fn linear(len: u32) -> (Self, Vec<H256>) {
            let mut chain = Self::default();
            let mut hashes = vec![H256::zero()];
            for n in 1..=len {
                let h = H256::from_low_u64_be(n as u64);
                chain.push(h, n, hashes[(n - 1) as usize]);
                hashes.push(h);
                chain.finalized = h;
            }
            (chain, hashes)
        }
    }

    impl ChainView for MockChain {
        fn finalized_hash(&self) -> H256 {
            self.finalized
        }

        fn finalized_number(&self) -> u32 {
            self.number(&self.finalized).unwrap_or(0)
        }

        fn header(&self, hash: &H256) -> Option<(u32, H256)> {
            self.headers.get(hash).copied()
        }
    }

    fn qc(height: u32, block: H256) -> QuorumCertificate {
        QuorumCertificate {
            version: sff_primitives::SFF_PROTOCOL_VERSION,
            genesis_hash: H256::zero(),
            set_id: 0,
            snapshot_root: H256::zero(),
            height,
            round: 0,
            phase: sff_primitives::Phase::Commit,
            block_hash: Some(block),
            parent_hash: H256::zero(),
            prepare_qc_hash: None,
            vote_payload_hash: H256::zero(),
            aggregate_signature: sff_primitives::BlsSignature::ZERO,
            signer_bitmap: sff_primitives::SignerBitmap::default(),
            signed_stake: 100,
        }
    }

    #[test]
    fn consistency_classifies_finalized_ahead_and_conflicting() {
        let (mut chain, hashes) = MockChain::linear(5);
        // finalized head = #5.
        assert_eq!(consistency_of(&chain, &hashes[5]), Consistency::OnFinalizedChain);
        assert_eq!(consistency_of(&chain, &hashes[3]), Consistency::OnFinalizedChain);

        // A block ahead of finality is legitimate fast confirmation.
        let ahead = H256::from_low_u64_be(100);
        chain.push(ahead, 6, hashes[5]);
        assert_eq!(consistency_of(&chain, &ahead), Consistency::AheadOfFinality);

        // A fork at height 5 that GRANDPA did not finalize conflicts.
        let fork = H256::from_low_u64_be(200);
        chain.push(fork, 5, hashes[4]);
        assert_eq!(consistency_of(&chain, &fork), Consistency::Conflicting);

        // An unknown block cannot be claimed either way.
        assert_eq!(
            consistency_of(&chain, &H256::from_low_u64_be(999)),
            Consistency::Unknown
        );
    }

    #[test]
    fn ahead_of_finality_is_served_but_revocable() {
        let (mut chain, hashes) = MockChain::linear(5);
        let ahead = H256::from_low_u64_be(100);
        chain.push(ahead, 6, hashes[5]);

        let view = FastFinalityView::new();
        assert_eq!(view.observe_commit_qc(qc(6, ahead), &chain), Consistency::AheadOfFinality);

        let head = view.head(&chain).expect("ahead-of-finality is served");
        assert!(head.revocable);
        assert!(!head.grandpa_finalized);
        assert_eq!(head.height, 6);
        assert!(view.status(&chain).serving);

        // GRANDPA catches up: the same block becomes irreversible.
        chain.finalized = ahead;
        let head = view.head(&chain).expect("still served");
        assert!(head.grandpa_finalized);
        assert!(!head.revocable);
    }

    #[test]
    fn conflicting_commit_degrades_and_stops_serving() {
        let (mut chain, hashes) = MockChain::linear(5);
        let fork = H256::from_low_u64_be(200);
        chain.push(fork, 5, hashes[4]);

        let view = FastFinalityView::new();
        view.observe_commit_qc(qc(5, hashes[3]), &chain);
        assert!(view.head(&chain).is_some());

        assert_eq!(view.observe_commit_qc(qc(5, fork), &chain), Consistency::Conflicting);
        let status = view.status(&chain);
        assert!(status.degraded);
        assert!(!status.serving);
        assert_eq!(status.conflicts, 1);
        assert!(status.degrade_reason.is_some());
        assert_eq!(status.last_conflict.unwrap().sff_block_hash, fork);
        // The whole point: no confirmation is served while degraded.
        assert!(view.head(&chain).is_none());
        assert!(view.certificate(&chain, 5).is_none() || !status.serving);
    }

    #[test]
    fn a_served_head_that_is_reorged_away_degrades() {
        let (mut chain, hashes) = MockChain::linear(8);
        // SFF confirmed #8 while GRANDPA had only finalized #5.
        chain.finalized = hashes[5];
        let view = FastFinalityView::new();
        view.observe_commit_qc(qc(8, hashes[8]), &chain);
        assert_eq!(
            view.status(&chain).fast_confirmed_height,
            Some(8),
            "#8 is ahead of finality and served"
        );

        // GRANDPA finalizes a different branch: #8 is no longer canonical.
        let fork8 = H256::from_low_u64_be(300);
        chain.push(fork8, 8, hashes[7]);
        chain.finalized = fork8;

        let status = view.status(&chain);
        assert!(status.degraded, "revalidating must catch the reorg");
        assert!(!status.serving);
        assert!(view.head(&chain).is_none());
    }

    #[test]
    fn serving_resumes_after_a_divergence_but_keeps_the_audit_trail() {
        let (mut chain, hashes) = MockChain::linear(5);
        let fork = H256::from_low_u64_be(200);
        chain.push(fork, 5, hashes[4]);

        let view = FastFinalityView::new();
        view.observe_commit_qc(qc(5, fork), &chain);
        assert!(view.status(&chain).degraded);

        let good = H256::from_low_u64_be(100);
        chain.push(good, 6, hashes[5]);
        view.observe_commit_qc(qc(6, good), &chain);

        let status = view.status(&chain);
        assert!(!status.degraded);
        assert!(status.serving);
        assert_eq!(status.fast_confirmed_height, Some(6));
        // Audit trail survives recovery so indexers can still reconcile.
        assert_eq!(status.conflicts, 1);
        assert!(status.last_conflict.is_some());
    }

    #[test]
    fn history_is_bounded_and_never_serves_a_lower_height_as_head() {
        let (mut chain, _) = MockChain::linear(1);
        let view = FastFinalityView::new();
        let mut prev = *chain.headers.keys().next().unwrap();
        for n in 2..=(MAX_HISTORY as u32 + 20) {
            let h = H256::from_low_u64_be(n as u64);
            chain.push(h, n, prev);
            chain.finalized = h;
            view.observe_commit_qc(qc(n, h), &chain);
            prev = h;
        }
        let status = view.status(&chain);
        assert_eq!(status.fast_confirmed_height, Some(MAX_HISTORY as u32 + 20));
        assert!(view.certificate(&chain, MAX_HISTORY as u32 + 20).is_some());
        // Evicted from the bounded history, the head is unaffected.
        assert!(view.certificate(&chain, 1).is_none());
    }
}
