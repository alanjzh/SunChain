//! Bounded in-memory pool for SFF votes, certificates, pending rounds and the
//! two-phase lock tracker (design §5 `fast_finality_pool.rs`).
//!
//! Every collection here is bounded; growth is capped, never hoped against. The
//! pool is shared (`Arc<Mutex>`) between the network gossip (which inserts
//! validated messages) and the voter/aggregator/observer (which read and
//! aggregate). F2 is shadow: the pool never feeds fork choice or finality.

use std::collections::HashMap;

use sff_crypto::{aggregate_votes, LockTracker};
use sff_primitives::{exceeds_two_thirds, FinalityAuthoritySet, Phase, QuorumCertificate, Vote};
use sp_core::H256;

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
    /// Total votes, for the cap.
    total_votes: usize,
}

impl FastFinalityPool {
    pub fn new() -> Self {
        Self::default()
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
        let entry = self
            .votes
            .entry((v.set_id, v.phase, v.height, v.round))
            .or_default();
        entry.insert(v.signer_index, v);
        self.seen_vote.insert(key, ());
        self.total_votes += 1;
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
        }
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
        let qc = aggregate_votes(&votes, set).ok()?;
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
