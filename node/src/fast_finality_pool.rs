//! Bounded in-memory pool for SFF votes, certificates, pending rounds and the
//! two-phase lock tracker (design §5 `fast_finality_pool.rs`).
//!
//! Every collection here is bounded; growth is capped, never hoped against. The
//! pool is shared (`Arc<Mutex>`) between the network gossip (which inserts
//! validated messages) and the voter/aggregator/observer (which read and
//! aggregate). On the GRANDPA-finality genesis profile F2 is shadow: the pool
//! never feeds fork choice or finality. With SFF selected as the genesis finality
//! axis (strategy doc Stage 3), CommitQCs from this pool become finality proofs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sff_crypto::{aggregate_votes_cached, verify_vote, LockTracker};
use sff_primitives::{exceeds_two_thirds, FinalityAuthoritySet, Phase, QuorumCertificate, Vote};
use sp_core::H256;
use tokio::sync::Notify;

/// Hard cap on total collected votes, across all (phase, height, round) keys.
const MAX_TOTAL_VOTES: usize = 16 * 1024;
/// Hard cap on collected certificates.
const MAX_TOTAL_QCS: usize = 4 * 1024;
/// Hard cap for CommitQCs that arrived before their matching PrepareQC.
const MAX_PENDING_COMMIT_QCS: usize = 4 * 1024;

#[derive(Default)]
pub struct FastFinalityPool {
    /// `(phase, height, round) -> signer_index -> vote`.
    votes: HashMap<(u64, Phase, u32, u32), HashMap<u32, Vote>>,
    /// Best certificate per `(phase, height, round)`.
    qcs: HashMap<(u64, Phase, u32, u32), QuorumCertificate>,
    pending_commit_qcs: HashMap<(u64, u32, u32, H256, H256), QuorumCertificate>,
    /// `height ->` best CommitQC (the SFF finality candidate).
    commit_qc: HashMap<(u64, u32), QuorumCertificate>,
    /// Two-phase lock state machine.
    locks: HashMap<u64, LockTracker>,
    /// `(phase, height, round, signer_index) -> ()` to de-dupe votes.
    seen_vote: HashMap<(u64, Phase, u32, u32, u32), ()>,
    /// `(set_id, phase, height, round, signer_index) -> ()`: votes whose BLS
    /// signature+PoP already passed `verify_vote`. `try_build_qc` verifies a
    /// vote once and memoizes it here, so `aggregate_votes_cached` skips
    /// re-verification on subsequent attempts. The stored votes are immutable
    /// (duplicates are dropped before insertion), so the memo is sound. Bounded
    /// by `prune_below`.
    verified: HashSet<(u64, Phase, u32, u32, u32)>,
    /// Total votes, for the cap.
    total_votes: usize,
    /// Keys whose vote set changed since the last aggregation attempt.
    ///
    /// Aggregation costs one BLS verification per vote, so it must happen when
    /// something changed — not on every timer tick for every pending height.
    /// Without this the per-tick cost grows with the backlog, which makes the
    /// confirmation frontier fall further behind the faster it needs to run.
    dirty: HashSet<(u64, Phase, u32, u32)>,
    /// CommitQCs recorded since the observer last ran.
    new_commit_qcs: Vec<QuorumCertificate>,
    /// Lowest block height still accepted/pruned-away below. 0 = unbounded.
    floor: u32,
    /// Wakes the engine when something was inserted (event-driven aggregation).
    waker: Option<Arc<Notify>>,
}

impl FastFinalityPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the engine's wake-up handle: every accepted insert notifies it,
    /// so aggregation is driven by events instead of by the timer alone.
    pub fn set_waker(&mut self, waker: Arc<Notify>) {
        self.waker = Some(waker);
    }

    /// Lowest height still accepted. Votes for older heights are dropped: a
    /// shadow confirmation for a block far behind the tip is worth nothing and
    /// would only grow the aggregation backlog.
    pub fn set_floor(&mut self, floor: u32) {
        self.floor = floor;
    }

    /// Drops everything below `min_height` (votes, certificates, dedup keys).
    pub fn prune_below(&mut self, min_height: u32) {
        let dropped: Vec<(u64, Phase, u32, u32)> = self
            .votes
            .keys()
            .copied()
            .filter(|(_, _, height, _)| *height < min_height)
            .collect();
        for key in dropped {
            self.votes.remove(&key);
            self.dirty.remove(&key);
        }
        self.qcs.retain(|(_, _, height, _), _| *height >= min_height);
        self.pending_commit_qcs
            .retain(|(_, height, _, _, _), _| *height >= min_height);
        self.commit_qc.retain(|(_, height), _| *height >= min_height);
        self.seen_vote.retain(|(_, _, height, _, _), _| *height >= min_height);
        self.verified.retain(|(_, _, height, _, _)| *height >= min_height);
        self.dirty.retain(|(_, _, height, _)| *height >= min_height);
        self.total_votes = self.votes.values().map(|m| m.len()).sum();
    }

    /// Takes the keys whose vote set changed since the last call.
    ///
    /// Aggregation is BLS work, so callers must iterate **this** rather than all
    /// pending keys.
    pub fn take_dirty(&mut self) -> Vec<(u64, Phase, u32, u32)> {
        self.dirty.drain().collect()
    }

    /// CommitQCs recorded since the last call (drains the queue).
    pub fn take_new_commit_qcs(&mut self) -> Vec<QuorumCertificate> {
        std::mem::take(&mut self.new_commit_qcs)
    }

    fn notify(&self) {
        if let Some(waker) = &self.waker {
            waker.notify_one();
        }
    }

    /// Inserts a vote if not already seen. Returns `true` if newly inserted.
    pub fn add_vote(&mut self, v: Vote) -> bool {
        let key = (v.set_id, v.phase, v.height, v.round, v.signer_index);
        if self.seen_vote.contains_key(&key) {
            return false;
        }
        if self.total_votes >= MAX_TOTAL_VOTES {
            return false;
        }
        if v.height < self.floor {
            return false;
        }
        let agg_key = (v.set_id, v.phase, v.height, v.round);
        let entry = self.votes.entry(agg_key).or_default();
        entry.insert(v.signer_index, v);
        self.seen_vote.insert(key, ());
        self.total_votes += 1;
        self.dirty.insert(agg_key);
        self.notify();
        true
    }

    /// Inserts a certificate if it improves on (or equals) what we already have.
    /// Updates the lock tracker and records CommitQCs. Returns `true` if changed.
    pub fn add_qc(&mut self, qc: QuorumCertificate) -> bool {
        let key = (qc.set_id, qc.phase, qc.height, qc.round);
        match self.qcs.get(&key) {
            Some(existing) if existing.signed_stake >= qc.signed_stake => return false,
            _ => {}
        }
        if self.qcs.len() >= MAX_TOTAL_QCS {
            return false;
        }
        // Do not surface certificates that violate the two-phase lock.  F2 is
        // shadow-only, but its telemetry must still reflect valid protocol state.
        match qc.phase {
            Phase::Prepare => {
                if self
                    .locks
                    .entry(qc.set_id)
                    .or_default()
                    .observe_prepare_qc(&qc)
                    .is_err()
                {
                    return false;
                }
            }
            Phase::Commit => {
                let Some(prepare) = self
                    .qcs
                    .get(&(qc.set_id, Phase::Prepare, qc.height, qc.round))
                else {
                    let (Some(block), Some(prepare_hash)) = (qc.block_hash, qc.prepare_qc_hash)
                    else {
                        return false;
                    };
                    let pending_key = (qc.set_id, qc.height, qc.round, block, prepare_hash);
                    if self.pending_commit_qcs.len() >= MAX_PENDING_COMMIT_QCS
                        && !self.pending_commit_qcs.contains_key(&pending_key)
                    {
                        return false;
                    }
                    self.pending_commit_qcs.entry(pending_key).or_insert(qc);
                    return true;
                };
                if prepare.block_hash != qc.block_hash
                    || qc.prepare_qc_hash != Some(prepare.vote_payload_hash)
                {
                    return false;
                }
                if self
                    .locks
                    .entry(qc.set_id)
                    .or_default()
                    .observe_commit_qc(&qc)
                    .is_err()
                {
                    return false;
                }
            }
            Phase::Skip => {}
        }
        if qc.phase == Phase::Commit {
            self.commit_qc
                .entry((qc.set_id, qc.height))
                .or_insert_with(|| qc.clone());
            self.new_commit_qcs.push(qc.clone());
        }
        // Wake the engine on every accepted certificate (pipeline overlap, §8.2):
        // a *received* Prepare/Skip QC must drive our Commit vote / round advance
        // immediately, not wait for the aggregation timer. Locally built QCs call
        // `on_built_qc` directly; this is the backstop for QCs learned only via gossip.
        self.notify();
        self.qcs.insert(key, qc);
        if key.1 == Phase::Prepare {
            let pending_keys: Vec<_> = self
                .pending_commit_qcs
                .keys()
                .filter(|(set_id, height, round, _, _)| {
                    *set_id == key.0 && *height == key.2 && *round == key.3
                })
                .copied()
                .collect();
            for pending_key in pending_keys {
                let pending = self
                    .pending_commit_qcs
                    .remove(&pending_key)
                    .expect("key was collected");
                self.add_qc(pending);
            }
        }
        true
    }

    /// Votes collected for `(phase, height, round)`.
    pub fn votes_for(&self, set_id: u64, phase: Phase, height: u32, round: u32) -> Vec<&Vote> {
        self.votes
            .get(&(set_id, phase, height, round))
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// The current best certificate for `(phase, height, round)`, if any.
    pub fn qc_for(
        &self,
        set_id: u64,
        phase: Phase,
        height: u32,
        round: u32,
    ) -> Option<&QuorumCertificate> {
        self.qcs.get(&(set_id, phase, height, round))
    }

    /// The best CommitQC for `height`, if any.
    pub fn commit_qc_for(&self, set_id: u64, height: u32) -> Option<&QuorumCertificate> {
        self.commit_qc.get(&(set_id, height))
    }

    /// All `(set_id, height, round, qc)` for the given phase, for re-gossip / repair
    /// enumeration. Bounded by the pool itself, so the caller cannot grow it.
    pub fn qc_entries(
        &self,
        phase: Phase,
    ) -> Vec<(u64, u32, u32, QuorumCertificate)> {
        self.qcs
            .iter()
            .filter(|((_, p, _, _), _)| *p == phase)
            .map(|((set_id, _, height, round), v)| (*set_id, *height, *round, v.clone()))
            .collect()
    }

    /// All `(phase, height, round)` keys that currently hold at least one vote.
    pub fn votes_keys(&self) -> Vec<(u64, Phase, u32, u32)> {
        self.votes.keys().copied().collect()
    }

    /// Snapshot of all observed CommitQCs, for the observer sweep.
    pub fn commit_qc_entries(&self) -> Vec<(u64, u32, QuorumCertificate)> {
        self.commit_qc
            .iter()
            .map(|((set_id, height), v)| (*set_id, *height, v.clone()))
            .collect()
    }

    /// The lock tracker's committed block at `height`, if any.
    pub fn committed_at(&self, height: u32) -> Option<H256> {
        self.locks
            .values()
            .find_map(|lock| lock.committed_at(height))
    }

    /// Attempts to aggregate the collected votes for `(phase, height, round)`
    /// into a `> 2/3` certificate. Returns the QC if one was formed (and inserts
    /// it into the pool). Returns `None` if no QC is ready or already present.
    pub fn try_build_qc(
        &mut self,
        set: &FinalityAuthoritySet,
        phase: Phase,
        height: u32,
        round: u32,
    ) -> Option<QuorumCertificate> {
        if self.qcs.contains_key(&(set.set_id, phase, height, round)) {
            return None;
        }
        let votes: Vec<Vote> = match self.votes.get(&(set.set_id, phase, height, round)) {
            Some(m) if !m.is_empty() => m.values().cloned().collect(),
            _ => return None,
        };
        // Verify each vote at most once. A vote already in `self.verified` is
        // skipped; a freshly verified one is recorded so later aggregation
        // attempts over the same stored votes do not re-run the BLS pairing.
        let pre_verified: Vec<bool> = votes
            .iter()
            .map(|v| {
                let key = (v.set_id, v.phase, v.height, v.round, v.signer_index);
                if self.verified.contains(&key) {
                    true
                } else {
                    match verify_vote(v, set) {
                        Ok(()) => {
                            self.verified.insert(key);
                            true
                        }
                        Err(_) => false,
                    }
                }
            })
            .collect();
        let qc = aggregate_votes_cached(&votes, set, &pre_verified).ok()?;
        if !exceeds_two_thirds(qc.signed_stake, set.total_stake) {
            return None;
        }
        self.add_qc(qc.clone()).then_some(qc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sff_primitives::{BlsSignature, SignerBitmap, SFF_PROTOCOL_VERSION};

    fn qc(
        set_id: u64,
        phase: Phase,
        height: u32,
        block: H256,
        prepare: Option<H256>,
    ) -> QuorumCertificate {
        let mut qc = QuorumCertificate {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash: H256::repeat_byte(1),
            set_id,
            snapshot_root: H256::repeat_byte(2),
            height,
            round: 0,
            phase,
            block_hash: Some(block),
            parent_hash: H256::repeat_byte(3),
            prepare_qc_hash: prepare,
            vote_payload_hash: H256::zero(),
            aggregate_signature: BlsSignature::ZERO,
            signer_bitmap: SignerBitmap::with_capacity(1),
            signed_stake: 1,
        };
        qc.vote_payload_hash = qc.payload_hash();
        qc
    }

    fn vote(set_id: u64) -> Vote {
        Vote {
            version: SFF_PROTOCOL_VERSION,
            genesis_hash: H256::repeat_byte(1),
            set_id,
            snapshot_root: H256::repeat_byte(2),
            height: 10,
            round: 0,
            phase: Phase::Prepare,
            block_hash: Some(H256::repeat_byte(4)),
            parent_hash: H256::repeat_byte(3),
            prepare_qc_hash: None,
            signer_index: 0,
            signature: BlsSignature::ZERO,
        }
    }

    fn vote_at(set_id: u64, height: u32, signer: u32) -> Vote {
        let mut v = vote(set_id);
        v.height = height;
        v.signer_index = signer;
        v
    }

    /// Aggregation is BLS work: it must be attempted when the vote set changed,
    /// not for every pending height on every tick (§13.2(b)).
    #[test]
    fn dirty_keys_track_only_changed_vote_sets() {
        let mut pool = FastFinalityPool::new();
        assert!(pool.take_dirty().is_empty());

        assert!(pool.add_vote(vote_at(1, 10, 0)));
        assert!(pool.add_vote(vote_at(1, 10, 1)));
        // One key, whatever the number of votes that landed on it.
        assert_eq!(pool.take_dirty(), vec![(1, Phase::Prepare, 10, 0)]);
        // Drained: a second sweep must not redo the aggregation.
        assert!(pool.take_dirty().is_empty());

        // A duplicate vote (same signer) changes nothing.
        assert!(!pool.add_vote(vote_at(1, 10, 0)));
        assert!(pool.take_dirty().is_empty());
    }

    #[test]
    fn backlog_floor_rejects_and_prunes_old_heights() {
        let mut pool = FastFinalityPool::new();
        assert!(pool.add_vote(vote_at(1, 10, 0)));
        assert!(pool.add_vote(vote_at(1, 50, 0)));

        pool.set_floor(40);
        // Below the floor nothing is accepted any more.
        assert!(!pool.add_vote(vote_at(1, 30, 1)));

        pool.prune_below(40);
        assert!(pool.votes_for(1, Phase::Prepare, 10, 0).is_empty());
        assert_eq!(pool.votes_for(1, Phase::Prepare, 50, 0).len(), 1);
        // Pruned keys must not be handed back to the aggregator.
        assert_eq!(pool.take_dirty(), vec![(1, Phase::Prepare, 50, 0)]);
        // Re-inserting a pruned height is allowed (it was forgotten, not banned):
        // the floor is what rejects it.
        assert!(!pool.add_vote(vote_at(1, 10, 0)));
    }

    #[test]
    fn new_commit_qcs_are_drained_once() {
        let mut pool = FastFinalityPool::new();
        let prepare = qc(1, Phase::Prepare, 10, H256::repeat_byte(4), None);
        assert!(pool.add_qc(prepare.clone()));

        let commit = qc(
            1,
            Phase::Commit,
            10,
            H256::repeat_byte(4),
            Some(prepare.vote_payload_hash),
        );
        assert!(pool.add_qc(commit.clone()));

        let drained = pool.take_new_commit_qcs();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].phase, Phase::Commit);
        // The observer runs on every aggregation: draining twice must yield
        // nothing, otherwise the same QC is classified again and again.
        assert!(pool.take_new_commit_qcs().is_empty());
    }

    #[test]
    fn vote_deduplication_is_scoped_to_the_authority_set() {
        let mut pool = FastFinalityPool::new();
        assert!(pool.add_vote(vote(1)));
        assert!(pool.add_vote(vote(2)));
        assert_eq!(pool.votes_for(1, Phase::Prepare, 10, 0).len(), 1);
        assert_eq!(pool.votes_for(2, Phase::Prepare, 10, 0).len(), 1);
    }

    #[test]
    fn commit_requires_the_exact_prepare_qc() {
        let mut pool = FastFinalityPool::new();
        let prepare = qc(1, Phase::Prepare, 10, H256::repeat_byte(4), None);
        assert!(pool.add_qc(prepare.clone()));

        let wrong = qc(
            1,
            Phase::Commit,
            10,
            H256::repeat_byte(4),
            Some(H256::repeat_byte(9)),
        );
        assert!(!pool.add_qc(wrong));
        assert!(pool.commit_qc_for(1, 10).is_none());

        let commit = qc(
            1,
            Phase::Commit,
            10,
            H256::repeat_byte(4),
            Some(prepare.vote_payload_hash),
        );
        assert!(pool.add_qc(commit));
        assert!(pool.commit_qc_for(1, 10).is_some());
    }

    #[test]
    fn commit_waits_for_a_late_prepare_qc_then_is_checked() {
        let mut pool = FastFinalityPool::new();
        let prepare = qc(1, Phase::Prepare, 10, H256::repeat_byte(4), None);
        let commit = qc(
            1,
            Phase::Commit,
            10,
            H256::repeat_byte(4),
            Some(prepare.vote_payload_hash),
        );
        assert!(pool.add_qc(commit));
        assert!(pool.commit_qc_for(1, 10).is_none());
        assert!(pool.add_qc(prepare));
        assert!(pool.commit_qc_for(1, 10).is_some());
    }
}
