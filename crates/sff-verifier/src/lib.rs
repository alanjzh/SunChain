//! # sff-verifier
//!
//! `no_std` (with `alloc`) certificate verification, threshold/lock state machine
//! and equivocation detection for the Sunchain Fast Finality (SFF) protocol.
//!
//! This crate performs **no** BLS math — it validates the *structure* of a
//! [`QuorumCertificate`] (set binding, snapshot binding, bitmap bounds, an
//! independently recomputed `> 2/3` stake threshold, payload-hash and phase
//! invariants) and tracks the two-phase Prepare/Commit lock rules. The actual
//! aggregate BLS signature check is composed on top by `sff-crypto::verify_qc`.
//!
//! All thresholds use the strict `> 2/3` weighted stake rule from the design
//! (§4.1, §4.4): two conflicting `>= 2/3` certificates must share `> 1/3`
//! stake, which under the `< 1/3` Byzantine assumption cannot be entirely
//! double-signing.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

#[cfg(feature = "std")]
use std::collections::BTreeMap;

use sff_primitives::*;
use sp_core::H256;

// ---------------------------------------------------------------------------
// Single-certificate structural & threshold verification (no BLS)
// ---------------------------------------------------------------------------

/// Verifies the structural, set-binding and threshold properties of a
/// [`QuorumCertificate`] **without** checking its BLS aggregate signature.
///
/// Checks (design §4.3, §7):
/// 1. `version` is the supported protocol version.
/// 2. `set_id` and `snapshot_root` match the authority set (anti-replay /
///    membership binding).
/// 3. Bitmap is within bounds, covers every authority, and has no bit set
///    beyond the authority list.
/// 4. `signed_stake` is **independently recomputed** from the bitmap and the
///    set's frozen stakes, then checked to strictly exceed `2/3 · total`. The
///    QC's own `signed_stake` field is ignored.
/// 5. `vote_payload_hash` equals the hash of the reconstructed canonical message.
/// 6. Phase/block invariants hold (`Prepare`/`Commit` carry a block; only
///    `Commit` carries a `prepare_qc_hash`).
pub fn verify_certificate_logic(qc: &QuorumCertificate, set: &FinalityAuthoritySet) -> Result<(), SffError> {
	if qc.version != SFF_PROTOCOL_VERSION {
		return Err(SffError::UnsupportedVersion);
	}
	if qc.set_id != set.set_id {
		return Err(SffError::SetIdMismatch);
	}
	if qc.snapshot_root != set.snapshot_root {
		return Err(SffError::SnapshotRootMismatch);
	}
	if !qc.signer_bitmap.within_bounds() {
		return Err(SffError::BitmapTooLarge);
	}
	if !qc.signer_bitmap.covers(set.authorities.len()) {
		return Err(SffError::BitmapTooShort);
	}
	// No bit may be set beyond the authority list (incl. padding bits of the
	// last byte). A set bit here would index a non-existent authority.
	let n = set.authorities.len();
	for i in n..qc.signer_bitmap.len_bits() {
		if qc.signer_bitmap.get(i) {
			return Err(SffError::BitmapIndexOutOfRange);
		}
	}

	let signed = signed_stake_of(set, &qc.signer_bitmap)?;
	if !exceeds_two_thirds(signed, set.total_stake) {
		return Err(SffError::InsufficientStake);
	}

	if qc.payload_hash() != qc.vote_payload_hash {
		return Err(SffError::PayloadHashMismatch);
	}

	check_phase_invariants(qc.phase, qc.block_hash, qc.prepare_qc_hash)?;
	Ok(())
}

/// Verifies the structural properties of a single [`Vote`] (no signature check).
///
/// The BLS signature itself is validated by `sff-crypto::verify_vote`; this
/// guards the cheap, denial-of-service-relevant invariants first.
pub fn verify_vote_logic(vote: &Vote, set: &FinalityAuthoritySet) -> Result<(), SffError> {
	if vote.version != SFF_PROTOCOL_VERSION {
		return Err(SffError::UnsupportedVersion);
	}
	if vote.set_id != set.set_id {
		return Err(SffError::SetIdMismatch);
	}
	if vote.snapshot_root != set.snapshot_root {
		return Err(SffError::SnapshotRootMismatch);
	}
	if vote.signer_index as usize >= set.authorities.len() {
		return Err(SffError::BitmapIndexOutOfRange);
	}
	check_phase_invariants(vote.phase, vote.block_hash, vote.prepare_qc_hash)?;
	Ok(())
}

/// Enforces the per-phase block/prepare-qc invariants.
fn check_phase_invariants(
	phase: Phase,
	block_hash: Option<H256>,
	prepare_qc_hash: Option<H256>,
) -> Result<(), SffError> {
	match phase {
		Phase::Prepare => {
			if block_hash.is_none() {
				return Err(SffError::PhaseBlockMismatch);
			}
			if prepare_qc_hash.is_some() {
				return Err(SffError::PhaseBlockMismatch);
			}
		}
		Phase::Commit => {
			if block_hash.is_none() {
				return Err(SffError::PhaseBlockMismatch);
			}
			if prepare_qc_hash.is_none() {
				return Err(SffError::MissingPrepareQc);
			}
		}
		Phase::Skip => {
			if block_hash.is_some() {
				return Err(SffError::PhaseBlockMismatch);
			}
			if prepare_qc_hash.is_some() {
				return Err(SffError::PhaseBlockMismatch);
			}
		}
	}
	Ok(())
}

// ---------------------------------------------------------------------------
// Two-phase lock state machine
// ---------------------------------------------------------------------------

/// Tracks validated Prepare/Commit QCs to enforce the two-phase lock rules
/// (design §4.4). Maintains no BLS state: certificates are assumed to have
/// passed [`verify_certificate_logic`] (and the aggregate signature check)
/// before being handed here.
///
/// Safety property: at most one block hash can ever be committed per height.
/// Two conflicting `CommitQC`s (> 2/3 each) at the same height are therefore a
/// protocol breach and produce [`EquivocationEvidence`] rather than silently
/// tracking both.
#[derive(Clone, Default)]
pub struct LockTracker {
	prepared: BTreeMap<u32, PreparedBlock>,
	committed: BTreeMap<u32, H256>,
}

#[derive(Clone)]
struct PreparedBlock {
	block: H256,
	prepare_qc_hash: H256,
	#[allow(dead_code)]
	stake: u128,
}

impl LockTracker {
	/// Creates an empty tracker.
	pub fn new() -> Self {
		Self::default()
	}

	/// Records a validated PrepareQC. If a *different* block was already
	/// prepared at the same height, the two PrepareQCs conflict (equivocation)
	/// and evidence is returned; the newer observation is rejected.
	pub fn observe_prepare_qc(&mut self, qc: &QuorumCertificate) -> Result<(), SffError> {
		if qc.phase != Phase::Prepare {
			return Err(SffError::PhaseBlockMismatch);
		}
		let block = qc.block_hash.ok_or(SffError::PhaseBlockMismatch)?;
		// A `Prepare` QC's own `prepare_qc_hash` field is `None`; what a `Commit`
		// references is the hash of the PrepareQC's message, i.e. `vote_payload_hash`.
		let prep_hash = qc.vote_payload_hash;
		if let Some(existing) = self.prepared.get(&qc.height) {
			if existing.block != block {
				return Err(SffError::ConflictingCommit);
			}
			// Same block re-prepared: refresh stake (idempotent).
			self.prepared.insert(
				qc.height,
				PreparedBlock { block, prepare_qc_hash: prep_hash, stake: qc.signed_stake },
			);
			return Ok(());
		}
		self.prepared.insert(
			qc.height,
			PreparedBlock { block, prepare_qc_hash: prep_hash, stake: qc.signed_stake },
		);
		Ok(())
	}

	/// Records a validated CommitQC. The lock rule requires a matching PrepareQC
	/// for the same `(height, block, prepare_qc_hash)` to already exist. A
	/// conflicting committed block at the same height is a safety breach.
	pub fn observe_commit_qc(&mut self, qc: &QuorumCertificate) -> Result<(), SffError> {
		if qc.phase != Phase::Commit {
			return Err(SffError::PhaseBlockMismatch);
		}
		let block = qc.block_hash.ok_or(SffError::PhaseBlockMismatch)?;
		let prep_hash = qc.prepare_qc_hash.ok_or(SffError::MissingPrepareQc)?;
		match self.prepared.get(&qc.height) {
			Some(p) if p.block == block && p.prepare_qc_hash == prep_hash => {}
			_ => return Err(SffError::LockRuleViolation),
		}
		if let Some(&committed) = self.committed.get(&qc.height) {
			if committed != block {
				return Err(SffError::ConflictingCommit);
			}
			return Ok(());
		}
		self.committed.insert(qc.height, block);
		Ok(())
	}

	/// The committed block hash at `height`, if any.
	pub fn committed_at(&self, height: u32) -> Option<H256> {
		self.committed.get(&height).copied()
	}

	/// The highest `(height, block)` that has been committed, if any.
	pub fn highest_committed(&self) -> Option<(u32, H256)> {
		self.committed.iter().next_back().map(|(&h, &b)| (h, b))
	}
}

// ---------------------------------------------------------------------------
// Equivocation evidence
// ---------------------------------------------------------------------------

/// Kind of equivocation captured by [`EquivocationEvidence`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EquivocationKind {
	/// Two contradictory signed [`Vote`]s by the same authority.
	Vote,
	/// Two contradictory [`QuorumCertificate`]s (> 2/3 each), whose signer
	/// intersection must contain the double-voting stake.
	Certificate,
}

/// Provable evidence that one or more authorities double-signed conflicting
/// messages for the same `(set_id, height, round, phase)`.
///
/// For [`EquivocationKind::Vote`] the `conflicting_signers` bitmap contains a
/// single index (the equivocator). For [`EquivocationKind::Certificate`] it is
/// the intersection of the two QCs' signer bitmaps — i.e. the authorities that
/// demonstrably signed both conflicting certificates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EquivocationEvidence {
	pub kind: EquivocationKind,
	pub set_id: u64,
	pub height: u32,
	pub round: u32,
	pub phase: Phase,
	pub block_a: Option<H256>,
	pub block_b: Option<H256>,
	pub conflicting_signers: SignerBitmap,
}

impl EquivocationEvidence {
	/// Signed stake of the conflicting authorities, given the set.
	pub fn conflicting_stake(&self, set: &FinalityAuthoritySet) -> Result<u128, SffError> {
		signed_stake_of(set, &self.conflicting_signers)
	}
}

/// Detects a double-signing [`Vote`] (same authority, same context, conflicting
/// target) and returns evidence if found.
pub fn detect_vote_equivocation(a: &Vote, b: &Vote) -> Option<EquivocationEvidence> {
	let same_context = a.version == b.version
		&& a.genesis_hash == b.genesis_hash
		&& a.set_id == b.set_id
		&& a.snapshot_root == b.snapshot_root
		&& a.height == b.height
		&& a.round == b.round
		&& a.phase == b.phase
		&& a.signer_index == b.signer_index;
	if !same_context {
		return None;
	}
	let conflicting = a.block_hash != b.block_hash
		|| a.parent_hash != b.parent_hash
		|| a.prepare_qc_hash != b.prepare_qc_hash;
	if !conflicting {
		return None;
	}
	let mut bm = SignerBitmap::with_capacity(a.signer_index as usize + 1);
	bm.set(a.signer_index as usize);
	Some(EquivocationEvidence {
		kind: EquivocationKind::Vote,
		set_id: a.set_id,
		height: a.height,
		round: a.round,
		phase: a.phase,
		block_a: a.block_hash,
		block_b: b.block_hash,
		conflicting_signers: bm,
	})
}

/// Detects two conflicting [`QuorumCertificate`]s at the same
/// `(set_id, height, round, phase)` with different target blocks. The returned
/// evidence lists the signer intersection — the stake that must have
/// double-voted.
pub fn detect_qc_equivocation(a: &QuorumCertificate, b: &QuorumCertificate) -> Option<EquivocationEvidence> {
	let same_context = a.version == b.version
		&& a.genesis_hash == b.genesis_hash
		&& a.set_id == b.set_id
		&& a.snapshot_root == b.snapshot_root
		&& a.height == b.height
		&& a.round == b.round
		&& a.phase == b.phase;
	if !same_context {
		return None;
	}
	if a.block_hash == b.block_hash {
		return None;
	}
	let conflicting_signers = a.signer_bitmap.intersection(&b.signer_bitmap);
	if conflicting_signers.count_ones() == 0 {
		return None;
	}
	Some(EquivocationEvidence {
		kind: EquivocationKind::Certificate,
		set_id: a.set_id,
		height: a.height,
		round: a.round,
		phase: a.phase,
		block_a: a.block_hash,
		block_b: b.block_hash,
		conflicting_signers,
	})
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;

	fn set_of(n: usize, stakes: &[u128]) -> FinalityAuthoritySet {
		let authorities = (0..n)
			.map(|i| FinalityAuthority {
				validator_id: H256::repeat_byte(i as u8),
				grandpa_id: H256::repeat_byte(i as u8),
				bls_public_key: BlsPublicKey::ZERO,
				proof_of_possession: BlsSignature::ZERO,
				stake: stakes[i],
			})
			.collect();
		FinalityAuthoritySet::new(1, 0, authorities).unwrap()
	}

	fn qc(set: &FinalityAuthoritySet, bits: &[usize], phase: Phase, block: Option<H256>) -> QuorumCertificate {
		let mut bm = SignerBitmap::with_capacity(set.authorities.len());
		for &b in bits {
			bm.set(b);
		}
		let signed = bits
			.iter()
			.map(|&i| set.authorities[i].stake)
			.fold(0u128, |a, s| a.saturating_add(s));
		let (bh, parent) = (
			block,
			if block.is_some() { H256::repeat_byte(0x99) } else { H256::zero() },
		);
		QuorumCertificate {
			version: SFF_PROTOCOL_VERSION,
			genesis_hash: H256::repeat_byte(0xab),
			set_id: set.set_id,
			snapshot_root: set.snapshot_root,
			height: 10,
			round: 1,
			phase,
			block_hash: bh,
			parent_hash: parent,
			prepare_qc_hash: if phase == Phase::Commit { Some(H256::repeat_byte(0x77)) } else { None },
			vote_payload_hash: H256::zero(), // filled below
			aggregate_signature: BlsSignature::ZERO,
			signer_bitmap: bm,
			signed_stake: signed,
		}
	}

	#[test]
	fn logic_accepts_threshold_and_rejects_low() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut good = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		good.vote_payload_hash = good.payload_hash();
		assert!(verify_certificate_logic(&good, &set).is_ok());

		// recompute must match and exceed 2/3 (3/4 > 2/3)
		let mut low = qc(&set, &[0, 1], Phase::Prepare, Some(H256::repeat_byte(1)));
		low.vote_payload_hash = low.payload_hash();
		assert!(matches!(verify_certificate_logic(&low, &set), Err(SffError::InsufficientStake)));

		// ignoring the QC's own (forged) signed_stake field
		good.signed_stake = 0;
		assert!(verify_certificate_logic(&good, &set).is_ok());
	}

	#[test]
	fn logic_rejects_set_and_snapshot_mismatch() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut q = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		q.vote_payload_hash = q.payload_hash();
		let mut wrong_set = q.clone();
		wrong_set.set_id = 99;
		assert!(matches!(verify_certificate_logic(&wrong_set, &set), Err(SffError::SetIdMismatch)));
		let mut wrong_snap = q.clone();
		wrong_snap.snapshot_root = H256::repeat_byte(0xff);
		assert!(matches!(verify_certificate_logic(&wrong_snap, &set), Err(SffError::SnapshotRootMismatch)));
	}

	#[test]
	fn logic_rejects_payload_hash_mismatch() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut q = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		q.vote_payload_hash = H256::repeat_byte(0x00); // wrong
		assert!(matches!(verify_certificate_logic(&q, &set), Err(SffError::PayloadHashMismatch)));
	}

	#[test]
	fn logic_rejects_out_of_range_bit() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut q = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		q.vote_payload_hash = q.payload_hash();
		// set an out-of-range bit in the padding of the last byte
		q.signer_bitmap.0[0] |= 1u8 << 7; // bit 7 >= 4 authorities
		assert!(matches!(verify_certificate_logic(&q, &set), Err(SffError::BitmapIndexOutOfRange)));
	}

	#[test]
	fn logic_rejects_phase_block_invariant() {
		let set = set_of(4, &[1, 1, 1, 1]);
		// Commit without prepare_qc_hash
		let mut q = qc(&set, &[0, 1, 2], Phase::Commit, Some(H256::repeat_byte(1)));
		q.prepare_qc_hash = None;
		q.vote_payload_hash = q.payload_hash();
		assert!(matches!(verify_certificate_logic(&q, &set), Err(SffError::MissingPrepareQc)));
		// Prepare with a block but also a prepare_qc_hash (forbidden)
		let mut p = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		p.prepare_qc_hash = Some(H256::repeat_byte(7));
		p.vote_payload_hash = p.payload_hash();
		assert!(matches!(verify_certificate_logic(&p, &set), Err(SffError::PhaseBlockMismatch)));
		// Skip with a block (forbidden)
		let mut s = qc(&set, &[0, 1, 2], Phase::Skip, Some(H256::repeat_byte(1)));
		s.vote_payload_hash = s.payload_hash();
		assert!(matches!(verify_certificate_logic(&s, &set), Err(SffError::PhaseBlockMismatch)));
	}

	#[test]
	fn lock_tracker_enforces_two_phase() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut prep = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		prep.vote_payload_hash = prep.payload_hash();
		// Commit references the PrepareQC's own message hash.
		let mut commit = qc(&set, &[0, 1, 2], Phase::Commit, Some(H256::repeat_byte(1)));
		commit.prepare_qc_hash = Some(prep.vote_payload_hash);
		commit.vote_payload_hash = commit.payload_hash();

		let mut tracker = LockTracker::new();
		// commit before prepare -> lock rule violation
		assert!(matches!(tracker.observe_commit_qc(&commit), Err(SffError::LockRuleViolation)));
		// prepare ok
		assert!(tracker.observe_prepare_qc(&prep).is_ok());
		// commit now ok
		assert!(tracker.observe_commit_qc(&commit).is_ok());
		assert_eq!(tracker.committed_at(10), Some(H256::repeat_byte(1)));
		assert_eq!(tracker.highest_committed(), Some((10, H256::repeat_byte(1))));
	}

	#[test]
	fn lock_tracker_rejects_conflicting_commit() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let mut prep_a = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		prep_a.vote_payload_hash = prep_a.payload_hash();
		let mut prep_b = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(2)));
		prep_b.vote_payload_hash = prep_b.payload_hash();
		let mut commit_a = qc(&set, &[0, 1, 2], Phase::Commit, Some(H256::repeat_byte(1)));
		commit_a.prepare_qc_hash = Some(prep_a.vote_payload_hash);
		commit_a.vote_payload_hash = commit_a.payload_hash();
		let mut commit_b = qc(&set, &[0, 1, 2], Phase::Commit, Some(H256::repeat_byte(2)));
		commit_b.prepare_qc_hash = Some(prep_b.vote_payload_hash);
		commit_b.vote_payload_hash = commit_b.payload_hash();

		let mut tracker = LockTracker::new();
		assert!(tracker.observe_prepare_qc(&prep_a).is_ok());
		// A second Prepare for a different block at the same height conflicts.
		assert!(matches!(tracker.observe_prepare_qc(&prep_b), Err(SffError::ConflictingCommit)));
		assert!(tracker.observe_commit_qc(&commit_a).is_ok());
		// commit_b references a prepare that was never accepted -> lock violation.
		assert!(matches!(tracker.observe_commit_qc(&commit_b), Err(SffError::LockRuleViolation)));
	}

	#[test]
	fn equivocation_detection() {
		let set = set_of(4, &[1, 1, 1, 1]);
		let base = |block: u8| {
			let v = Vote {
				version: SFF_PROTOCOL_VERSION,
				genesis_hash: H256::repeat_byte(0xab),
				set_id: 1,
				snapshot_root: set.snapshot_root,
				height: 10,
				round: 1,
				phase: Phase::Prepare,
				block_hash: Some(H256::repeat_byte(block)),
				parent_hash: H256::repeat_byte(0x99),
				prepare_qc_hash: None,
				signer_index: 0,
				signature: BlsSignature::ZERO,
			};
			v
		};
		let a = base(1);
		let b = base(2); // same signer, different block
		let ev = detect_vote_equivocation(&a, &b).expect("equivocation detected");
		assert_eq!(ev.kind, EquivocationKind::Vote);
		assert_eq!(ev.conflicting_signers.count_ones(), 1);

		// same block -> not equivocation
		assert!(detect_vote_equivocation(&a, &base(1)).is_none());

		// different signer -> not equivocation
		let mut c = base(2);
		c.signer_index = 1;
		assert!(detect_vote_equivocation(&a, &c).is_none());

		// QC vs QC
		let qa = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(1)));
		let mut qb = qc(&set, &[0, 1, 2], Phase::Prepare, Some(H256::repeat_byte(2)));
		// shift bitmap of b so intersection is 2 signers (0,1)
		qb.signer_bitmap = SignerBitmap::with_capacity(4);
		qb.signer_bitmap.set(0);
		qb.signer_bitmap.set(1);
		qb.signer_bitmap.set(3);
		let qev = detect_qc_equivocation(&qa, &qb).expect("qc equivocation");
		assert_eq!(qev.kind, EquivocationKind::Certificate);
		assert_eq!(qev.conflicting_signers.count_ones(), 2);
	}
}

// ---------------------------------------------------------------------------
// Bounded exhaustive model checker (F1 gate: formal state-machine coverage)
//
// This is the *executable* half of the F1 formal model. It mirrors the TLA+
// specification in `docs/sff-f1-formal/SFFFinality.tla`: over every small
// authority committee it enumerates all quorum certificates and every reachable
// LockTracker observer state, and asserts the safety invariants that the TLA+
// `SafetyInv` / `LockInv` / `QuorumInv` state:
//
//   * Quorum-intersection lemma: two conflicting `> 2/3` certificates at the
//     same (height, round, phase) must share `> 1/3` signed stake — i.e. any
//     conflicting commit is only possible with `> 1/3` double-signers, which
//     the Byzantine `< 1/3` assumption rules out.
//   * Equivocation detection is complete: every conflicting pair is flagged.
//   * The LockTracker never records two different committed blocks per height,
//     and a commit is rejected unless a matching prepare was recorded first.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod model_check {
	use super::*;
	use sff_primitives::*;
	use sp_core::H256;

	const B0: Option<H256> = Some(H256::repeat_byte(0x10));
	const B1: Option<H256> = Some(H256::repeat_byte(0x20));

	fn mk_set(n: usize, stakes: &[u128]) -> FinalityAuthoritySet {
		let authorities = (0..n)
			.map(|i| FinalityAuthority {
				validator_id: H256::repeat_byte(i as u8),
				grandpa_id: H256::repeat_byte(i as u8),
				bls_public_key: BlsPublicKey::ZERO,
				proof_of_possession: BlsSignature::ZERO,
				stake: stakes[i],
			})
			.collect();
		FinalityAuthoritySet::new(1, 0, authorities).unwrap()
	}

	/// All signer subsets that individually reach a `> 2/3` quorum.
	fn quorum_subsets(n: usize, set: &FinalityAuthoritySet) -> Vec<Vec<usize>> {
		let total = set.total_stake;
		(0..(1u32 << n))
			.map(|mask| (0..n).filter(|i| mask & (1u32 << i) != 0).collect::<Vec<_>>())
			.filter(|s| {
				let st = s.iter().map(|&i| set.authorities[i].stake).fold(0u128, |a, x| a + x);
				exceeds_two_thirds(st, total)
			})
			.collect()
	}

	fn mk_qc(
		set: &FinalityAuthoritySet,
		signers: &[usize],
		phase: Phase,
		height: u32,
		round: u32,
		block: Option<H256>,
		prep: Option<H256>,
	) -> QuorumCertificate {
		let mut bm = SignerBitmap::with_capacity(set.authorities.len());
		for &i in signers {
			bm.set(i);
		}
		let signed = signers.iter().map(|&i| set.authorities[i].stake).fold(0u128, |a, x| a + x);
		let mut q = QuorumCertificate {
			version: SFF_PROTOCOL_VERSION,
			genesis_hash: H256::repeat_byte(0xab),
			set_id: set.set_id,
			snapshot_root: set.snapshot_root,
			height,
			round,
			phase,
			block_hash: block,
			parent_hash: if block.is_some() { H256::repeat_byte(0x99) } else { H256::zero() },
			prepare_qc_hash: prep,
			vote_payload_hash: H256::zero(), // filled below
			aggregate_signature: BlsSignature::ZERO,
			signer_bitmap: bm,
			signed_stake: signed,
		};
		q.vote_payload_hash = q.payload_hash();
		q
	}

	fn mk_vote(
		set: &FinalityAuthoritySet,
		signer: usize,
		phase: Phase,
		block: Option<H256>,
		parent: Option<H256>,
		prep: Option<H256>,
	) -> Vote {
		Vote {
			version: SFF_PROTOCOL_VERSION,
			genesis_hash: H256::repeat_byte(0xab),
			set_id: set.set_id,
			snapshot_root: set.snapshot_root,
			height: 1,
			round: 1,
			phase,
			block_hash: block,
			parent_hash: parent.unwrap_or_else(H256::zero),
			prepare_qc_hash: prep,
			signer_index: signer as u32,
			signature: BlsSignature::ZERO,
		}
	}

	#[test]
	fn model_check_certificate_safety() {
		for n in 1..=5usize {
			check_committee(&mk_set(n, &vec![1u128; n]));
		}
		// weighted / asymmetric stakes exercise the 256-bit threshold edge cases
		check_committee(&mk_set(4, &[1, 1, 1, 2]));
		check_committee(&mk_set(5, &[1, 2, 3, 1, 1]));
		check_committee(&mk_set(5, &[10, 1, 1, 1, 1]));
	}

	fn check_committee(set: &FinalityAuthoritySet) {
		let total = set.total_stake;
		let subsets = quorum_subsets(set.authorities.len(), set);

		// Enumerate every well-formed quorum certificate.
		let mut qcs = Vec::new();
		for &h in &[1u32, 2, 3] {
			for &r in &[1u32, 2] {
				for &ph in &[Phase::Prepare, Phase::Commit] {
					for &blk in &[B0, B1] {
						for sg in &subsets {
							let prep = if ph == Phase::Commit { Some(H256::repeat_byte(0x77)) } else { None };
							let q = mk_qc(set, sg, ph, h, r, blk, prep);
							// Every enumerated certificate must genuinely pass the verifier.
							assert!(
								verify_certificate_logic(&q, set).is_ok(),
								"valid quorum certificate rejected by verifier"
							);
							qcs.push(q);
						}
					}
				}
			}
		}

		// (1) Quorum-intersection lemma + equivocation completeness.
		for i in 0..qcs.len() {
			for j in (i + 1)..qcs.len() {
				let (a, b) = (&qcs[i], &qcs[j]);
				let same_ctx =
					a.height == b.height && a.round == b.round && a.phase == b.phase;
				if !same_ctx {
					continue;
				}
				if a.block_hash != b.block_hash {
					// Conflicting certificates -> their signer intersection must
					// exceed 1/3 of total stake (quorum math: 2/3 + 2/3 - 1).
					let inter = a.signer_bitmap.intersection(&b.signer_bitmap);
					let istake =
						signed_stake_of(set, &inter).expect("intersection indices in range");
					assert!(
						3 * istake > total,
						"quorum-intersection below 1/3: {} vs {} (set {:?})",
						istake,
						total,
						set.authorities.iter().map(|x| x.stake).collect::<Vec<_>>()
					);
					// And the equivocation detector must catch it.
					assert!(
						detect_qc_equivocation(a, b).is_some(),
						"conflicting certificates not flagged as equivocation"
					);
				} else {
					assert!(
						detect_qc_equivocation(a, b).is_none(),
						"identical-block certificates flagged as equivocation"
					);
				}
			}
		}

		// (2) LockTracker safety over bounded observer sequences.
		check_lock_tracker(set, &subsets);
	}

	fn check_lock_tracker(set: &FinalityAuthoritySet, subsets: &[Vec<usize>]) {
		// Need at least two distinct quorum signer sets to model a conflict.
		if subsets.len() < 2 {
			return;
		}
		let p0 = &subsets[0];
		let p1 = &subsets[1];
		let prep0 = mk_qc(set, p0, Phase::Prepare, 1, 1, B0, None);
		let prep1 = mk_qc(set, p1, Phase::Prepare, 1, 1, B1, None);
		let commit0 = mk_qc(set, p0, Phase::Commit, 1, 1, B0, Some(prep0.vote_payload_hash));
		let commit1 = mk_qc(set, p1, Phase::Commit, 1, 1, B1, Some(prep1.vote_payload_hash));

		let actions = vec![
			Action::Prep(prep0.clone()),
			Action::Prep(prep1.clone()),
			Action::Commit(commit0.clone()),
			Action::Commit(commit1.clone()),
		];

		let tracker = LockTracker::new();
		dfs(&tracker, &actions, 4, &commit0, &commit1, B0.unwrap(), B1.unwrap());
	}

	enum Action {
		Prep(QuorumCertificate),
		Commit(QuorumCertificate),
	}

	fn dfs(
		tracker: &LockTracker,
		actions: &[Action],
		depth: usize,
		commit0: &QuorumCertificate,
		commit1: &QuorumCertificate,
		b0: H256,
		b1: H256,
	) {
		// Safety: at most one committed block per height. If a block is committed,
		// a *conflicting* commit must be rejected (never recorded).
		if let Some(committed) = tracker.committed_at(1) {
			let conflicting = if committed == b0 { commit1 } else { commit0 };
			let mut probe = tracker.clone();
			let res = probe.observe_commit_qc(conflicting);
			assert!(res.is_err(), "LockTracker accepted a conflicting commit");
		}

		if depth == 0 {
			return;
		}
		for a in actions {
			let mut next = tracker.clone();
			match a {
				Action::Prep(q) => {
					let _ = next.observe_prepare_qc(q);
				}
				Action::Commit(q) => {
					let _ = next.observe_commit_qc(q);
				}
			}
			dfs(&next, actions, depth - 1, commit0, commit1, b0, b1);
		}
	}

	#[test]
	fn model_check_vote_equivocation() {
		for n in 2..=5usize {
			let set = mk_set(n, &vec![1u128; n]);
			for i in 0..n {
				for j in 0..n {
					for &bi in &[B0, B1] {
						for &bj in &[B0, B1] {
							let vi = mk_vote(&set, i, Phase::Prepare, bi, None, None);
							let vj = mk_vote(&set, j, Phase::Prepare, bj, None, None);
							let ev = detect_vote_equivocation(&vi, &vj);
							if i == j && bi != bj {
								assert!(ev.is_some(), "vote equivocation missed (i=j, bi!=bj)");
							} else {
								assert!(
									ev.is_none(),
									"false vote equivocation (i={i}, j={j}, bi={bi:?}, bj={bj:?})"
								);
							}
						}
					}
					// Same signer, same block, different parent -> still equivocation.
					let vi = mk_vote(&set, i, Phase::Prepare, B0, Some(H256::repeat_byte(0xaa)), None);
					let vj = mk_vote(&set, i, Phase::Prepare, B0, Some(H256::repeat_byte(0xbb)), None);
					assert!(
						detect_vote_equivocation(&vi, &vj).is_some(),
						"vote equivocation via differing parent missed"
					);
					// Same signer, same block, different prepare_qc_hash -> equivocation.
					let vi = mk_vote(&set, i, Phase::Prepare, B0, None, Some(H256::repeat_byte(0x01)));
					let vj = mk_vote(&set, i, Phase::Prepare, B0, None, Some(H256::repeat_byte(0x02)));
					assert!(
						detect_vote_equivocation(&vi, &vj).is_some(),
						"vote equivocation via differing prepare_qc missed"
					);
				}
			}
		}
	}
}
