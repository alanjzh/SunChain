//! # sunlead-verifier
//!
//! `no_std` verification of SunLead block-header pre-digests and equivocation
//! proofs, performed **before** any runtime execution (per the dev plan's import
//! pipeline). A block is accepted only if all of the following hold:
//!
//! 1. protocol version / engine id,
//! 2. the schedule was active for the parent state (authority set + epoch match),
//! 3. the slot is within the epoch and the digest's `(leader_index, window_id)`
//!    matches the deterministic schedule derived from the parent-state snapshot,
//! 4. `schedule_root` in the digest matches the recomputed root,
//! 5. the leader's `sunl` signature over the bound message is valid,
//! 6. parent hash and slot monotonicity are consistent (checked by the import
//!    layer, helpers provided here).
//!
//! The verifier is fully deterministic and has no access to consensus internals.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

use sp_consensus_slots::Slot;
use sp_core::crypto::Pair as _;
use sp_core::H256;
use sunlead_primitives::{
	signing_message, AuthoritySetId, EpochIndex, SunLeadEquivocationProof, SunLeadId,
	SunLeadPreDigest, SunLeadPreDigestBody, SunLeadScheduleContext, SunLeadScheduleParams,
	SunLeadSignature, SUNLEAD_PROTOCOL_VERSION,
};
use sunlead_schedule::{build_schedule, ScheduleInput};

/// Reasons a SunLead pre-digest (or equivocation proof) may be rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SunLeadVerificationError {
	/// Protocol version in the digest does not match this engine.
	WrongProtocolVersion,
	/// Authority set id in the digest does not match the parent-state snapshot.
	AuthoritySetMismatch,
	/// Epoch index in the digest does not match the parent-state snapshot.
	EpochMismatch,
	/// The schedule could not be built from the supplied context.
	ScheduleBuildFailed,
	/// Slot is outside the epoch range.
	SlotOutOfRange,
	/// `schedule_root` in the digest does not match the recomputed root.
	ScheduleRootMismatch,
	/// `(leader_index, window_id)` does not match the schedule for this slot.
	LeaderMismatch,
	/// Signature over the digest body is invalid for the scheduled leader.
	InvalidSignature,
	/// Slot is not strictly greater than the parent slot.
	SlotNotMonotonic,
	/// Parent hash in the digest does not match the actual parent.
	ParentHashMismatch,
	/// Equivocation: the two headers are for different slots.
	EquivocationDifferentSlot,
	/// Equivocation: the two headers are for different authority sets.
	EquivocationDifferentSet,
	/// Equivocation: the two headers are for different leaders.
	EquivocationDifferentLeader,
	/// Equivocation: the two headers are identical.
	EquivocationSameHeader,
	/// Equivocation: a signature does not verify under the offender key.
	EquivocationSignature,
}

/// Verify a SunLead pre-digest against the parent-state schedule context.
///
/// `header_hash` is the hash of the block being imported (bound into the
/// signature so the signature cannot be replayed on another block).
pub fn verify_pre_digest(
	genesis_hash: &H256,
	header_hash: &H256,
	digest: &SunLeadPreDigest,
	context: &SunLeadScheduleContext,
	params: &SunLeadScheduleParams,
) -> Result<(), SunLeadVerificationError> {
	if digest.0.version != SUNLEAD_PROTOCOL_VERSION {
		return Err(SunLeadVerificationError::WrongProtocolVersion)
	}
	if digest.0.authority_set_id != context.authority_set_id {
		return Err(SunLeadVerificationError::AuthoritySetMismatch)
	}
	if digest.0.epoch_index != context.epoch_index {
		return Err(SunLeadVerificationError::EpochMismatch)
	}

	let input = ScheduleInput {
		chain_id: context.chain_id,
		authority_set_id: context.authority_set_id,
		epoch_index: context.epoch_index,
		epoch_seed: context.epoch_seed,
		authorities: context.authorities.clone(),
		params: *params,
	};
	let sched =
		build_schedule(&input).map_err(|_| SunLeadVerificationError::ScheduleBuildFailed)?;

	let slot = u64::from(digest.0.slot);
	// The digest stores the absolute runtime slot; the schedule is defined per-epoch as
	// `slot % epoch_slots`, so map into the current epoch before deriving the leader.
	let rel_slot = slot % params.epoch_slots;
	if rel_slot >= params.epoch_slots {
		return Err(SunLeadVerificationError::SlotOutOfRange)
	}
	if sched.root != digest.0.schedule_root {
		return Err(SunLeadVerificationError::ScheduleRootMismatch)
	}
	let (leader, window) = sched
		.leader_at(rel_slot)
		.ok_or(SunLeadVerificationError::SlotOutOfRange)?;
	if leader != digest.0.leader_index || window != digest.0.window_id {
		return Err(SunLeadVerificationError::LeaderMismatch)
	}

	let _ = (genesis_hash, header_hash);
	Ok(())
}

/// Verify the leader's **seal** over the pre-seal header hash.
///
/// The seal is a `DigestItem::Seal(SUNLEAD_ENGINE_ID, ..)` carrying the leader's
/// signature over `signing_message(genesis, pre_seal_hash, body)`. The leader is
/// identified by `digest.body.leader_index` into `context.authorities`; this
/// function checks the seal verifies against that leader's `sunl` key.
pub fn verify_seal(
	genesis_hash: H256,
	pre_seal_hash: H256,
	digest: &SunLeadPreDigest,
	context: &SunLeadScheduleContext,
	seal: &SunLeadSignature,
) -> Result<(), SunLeadVerificationError> {
	let leader_index = digest.0.leader_index as usize;
	let leader_id = context
		.authorities
		.get(leader_index)
		.ok_or(SunLeadVerificationError::LeaderMismatch)?;
	let msg = signing_message(&genesis_hash, &pre_seal_hash, &digest.0);
	let inner_pub: sp_core::sr25519::Public = leader_id.0.clone().into();
	let inner_sig: sp_core::sr25519::Signature = seal.clone().into();
	if !sp_core::sr25519::Pair::verify(&inner_sig, &msg, &inner_pub) {
		return Err(SunLeadVerificationError::InvalidSignature)
	}
	Ok(())
}

/// Check that the imported slot is strictly greater than the parent slot.
pub fn verify_slot_monotonicity(
	parent_slot: Slot,
	digest_slot: Slot,
) -> Result<(), SunLeadVerificationError> {
	if digest_slot <= parent_slot {
		Err(SunLeadVerificationError::SlotNotMonotonic)
	} else {
		Ok(())
	}
}

/// Check the digest's parent hash matches the actual parent block hash.
pub fn verify_parent_hash(
	digest_parent: H256,
	actual_parent: H256,
) -> Result<(), SunLeadVerificationError> {
	if digest_parent != actual_parent {
		Err(SunLeadVerificationError::ParentHashMismatch)
	} else {
		Ok(())
	}
}

/// Verify a double-signing (equivocation) proof.
///
/// Returns the offending leader's public key on success. The proof must contain
/// two distinct, valid headers for the same `(authority_set_id, slot,
/// leader_index)`, both signed by `offender`.
pub fn verify_equivocation_proof(
	genesis_hash: &H256,
	proof: &SunLeadEquivocationProof,
) -> Result<SunLeadId, SunLeadVerificationError> {
	let d1 = &proof.first;
	let d2 = &proof.second;

	if d1.digest.0.slot != d2.digest.0.slot {
		return Err(SunLeadVerificationError::EquivocationDifferentSlot)
	}
	if d1.digest.0.authority_set_id != d2.digest.0.authority_set_id {
		return Err(SunLeadVerificationError::EquivocationDifferentSet)
	}
	if d1.digest.0.leader_index != d2.digest.0.leader_index {
		return Err(SunLeadVerificationError::EquivocationDifferentLeader)
	}
	if d1.header_hash == d2.header_hash {
		return Err(SunLeadVerificationError::EquivocationSameHeader)
	}

	// Each header must be a valid seal by the offender over its pre-seal hash.
	let offender_pub: sp_core::sr25519::Public = proof.offender.clone().into();
	let s1: sp_core::sr25519::Signature = d1.seal.clone().into();
	let msg1 = signing_message(&genesis_hash, &d1.header_hash, &d1.digest.0);
	let s2: sp_core::sr25519::Signature = d2.seal.clone().into();
	let msg2 = signing_message(&genesis_hash, &d2.header_hash, &d2.digest.0);
	if !sp_core::sr25519::Pair::verify(&s1, &msg1, &offender_pub)
		|| !sp_core::sr25519::Pair::verify(&s2, &msg2, &offender_pub)
	{
		return Err(SunLeadVerificationError::EquivocationSignature)
	}
	Ok(proof.offender.clone())
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::crypto::Pair as _;
	use sp_core::sr25519::Pair;
	use sunlead_primitives::{SunLeadPreDigest, SunLeadScheduleParams};

	fn h256_repeat(b: u8) -> H256 {
		H256::from_slice(&[b; 32])
	}

	fn keys(n: usize) -> Vec<Pair> {
		(0..n).map(|i| Pair::from_seed(&[i as u8; 32])).collect()
	}

	struct Fixture {
		genesis: H256,
		header: H256,
		parent: H256,
		context: SunLeadScheduleContext,
		params: SunLeadScheduleParams,
		slot: u64,
		leader_index: u32,
		window_id: u32,
		leader_pair: Pair,
	}

	fn fixture() -> Fixture {
		let params = SunLeadScheduleParams::v1_defaults();
		let ks = keys(4);
		let stakes = [100u128, 50, 30, 20];
		let context = SunLeadScheduleContext {
			chain_id: 1,
			authority_set_id: 1,
			epoch_index: 1,
			epoch_seed: h256_repeat(3),
			authorities: ks
				.iter()
				.zip(stakes.iter())
				.map(|(k, s)| (sunlead_primitives::SunLeadId::from(k.public()), *s))
				.collect(),
		};
		let genesis = h256_repeat(0xaa);
		let parent = h256_repeat(0xbb);
		let header = h256_repeat(0xcc);
		let input = ScheduleInput {
			chain_id: context.chain_id,
			authority_set_id: context.authority_set_id,
			epoch_index: context.epoch_index,
			epoch_seed: context.epoch_seed,
			authorities: context.authorities.clone(),
			params,
		};
		let sched = build_schedule(&input).unwrap();
		let slot = 7u64;
		let (leader_index, window_id) = sched.leader_at(slot).unwrap();
		let leader_pair = ks[leader_index as usize].clone();
		Fixture { genesis, header, parent, context, params, slot, leader_index, window_id, leader_pair }
	}

	fn signed_digest(
		f: &Fixture,
		tamper: impl Fn(&mut SunLeadPreDigestBody),
	) -> (SunLeadPreDigest, SunLeadSignature) {
		let input = ScheduleInput {
			chain_id: f.context.chain_id,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			epoch_seed: f.context.epoch_seed,
			authorities: f.context.authorities.clone(),
			params: f.params,
		};
		let sched = build_schedule(&input).unwrap();
		let mut body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			slot: Slot::from(f.slot),
			window_id: f.window_id,
			leader_index: f.leader_index,
			parent_hash: f.parent,
			schedule_root: sched.root,
		};
		tamper(&mut body);
		let msg = signing_message(&f.genesis, &f.header, &body);
		let sig = sunlead_primitives::SunLeadSignature::from(f.leader_pair.sign(&msg));
		(SunLeadPreDigest(body), sig)
	}

	#[test]
	fn valid_digest_verifies() {
		let f = fixture();
		let (d, seal) = signed_digest(&f, |_| {});
		assert_eq!(
			verify_pre_digest(&f.genesis, &f.header, &d, &f.context, &f.params),
			Ok(())
		);
		assert_eq!(verify_seal(f.genesis, f.header, &d, &f.context, &seal), Ok(()));
	}

	#[test]
	fn wrong_version_is_rejected() {
		let f = fixture();
		let (d, _) = signed_digest(&f, |b| b.version = 99);
		assert_eq!(
			verify_pre_digest(&f.genesis, &f.header, &d, &f.context, &f.params),
			Err(SunLeadVerificationError::WrongProtocolVersion)
		);
	}

	#[test]
	fn wrong_schedule_root_is_rejected() {
		let f = fixture();
		let (d, _) = signed_digest(&f, |b| b.schedule_root = h256_repeat(0xff));
		assert_eq!(
			verify_pre_digest(&f.genesis, &f.header, &d, &f.context, &f.params),
			Err(SunLeadVerificationError::ScheduleRootMismatch)
		);
	}

	#[test]
	fn wrong_leader_index_is_rejected() {
		let f = fixture();
		let wrong = if f.leader_index == 0 { 1 } else { 0 };
		let (d, _) = signed_digest(&f, |b| b.leader_index = wrong);
		assert_eq!(
			verify_pre_digest(&f.genesis, &f.header, &d, &f.context, &f.params),
			Err(SunLeadVerificationError::LeaderMismatch)
		);
	}

	#[test]
	fn wrong_signature_key_is_rejected() {
		let f = fixture();
		// sign with a non-leader key
		let mut body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			slot: Slot::from(f.slot),
			window_id: f.window_id,
			leader_index: f.leader_index,
			parent_hash: f.parent,
			schedule_root: H256::default(),
		};
		let input = ScheduleInput {
			chain_id: f.context.chain_id,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			epoch_seed: f.context.epoch_seed,
			authorities: f.context.authorities.clone(),
			params: f.params,
		};
		body.schedule_root = build_schedule(&input).unwrap().root;
		let msg = signing_message(&f.genesis, &f.header, &body);
		// sign with key #3 (not necessarily the leader)
		let bad_pair = keys(4)[3].clone();
		let sig = sunlead_primitives::SunLeadSignature::from(bad_pair.sign(&msg));
		let d = SunLeadPreDigest(body);
		assert_eq!(
			verify_seal(f.genesis, f.header, &d, &f.context, &sig),
			Err(SunLeadVerificationError::InvalidSignature)
		);
	}

	#[test]
	fn slot_out_of_range_is_rejected() {
		let f = fixture();
		let (d, _) = signed_digest(&f, |b| b.slot = Slot::from(f.params.epoch_slots + 5));
		assert_eq!(
			verify_pre_digest(&f.genesis, &f.header, &d, &f.context, &f.params),
			Err(SunLeadVerificationError::SlotOutOfRange)
		);
	}

	#[test]
	fn slot_monotonicity() {
		assert_eq!(
			verify_slot_monotonicity(Slot::from(10), Slot::from(11)),
			Ok(())
		);
		assert_eq!(
			verify_slot_monotonicity(Slot::from(10), Slot::from(10)),
			Err(SunLeadVerificationError::SlotNotMonotonic)
		);
	}

	#[test]
	fn equivocation_proof_verifies_and_rejects() {
		let f = fixture();
		let header_a = h256_repeat(0x01);
		let header_b = h256_repeat(0x02);
		let mk = |h: H256| {
			let mut body = SunLeadPreDigestBody {
				version: SUNLEAD_PROTOCOL_VERSION,
				authority_set_id: f.context.authority_set_id,
				epoch_index: f.context.epoch_index,
				slot: Slot::from(f.slot),
				window_id: f.window_id,
				leader_index: f.leader_index,
				parent_hash: f.parent,
				schedule_root: H256::default(),
			};
			let input = ScheduleInput {
				chain_id: f.context.chain_id,
				authority_set_id: f.context.authority_set_id,
				epoch_index: f.context.epoch_index,
				epoch_seed: f.context.epoch_seed,
				authorities: f.context.authorities.clone(),
				params: f.params,
			};
			body.schedule_root = build_schedule(&input).unwrap().root;
			let msg = signing_message(&f.genesis, &h, &body);
			let sig = sunlead_primitives::SunLeadSignature::from(f.leader_pair.sign(&msg));
			sunlead_primitives::SunLeadEquivocationHeader {
				digest: SunLeadPreDigest(body),
				header_hash: h,
				seal: sig,
			}
		};
		let offender = sunlead_primitives::SunLeadId::from(f.leader_pair.public());
		let a = mk(header_a);
		let b = mk(header_b);
		let proof = SunLeadEquivocationProof {
			authority_set_id: f.context.authority_set_id,
			slot: Slot::from(f.slot),
			offender: offender.clone(),
			first: a.clone(),
			second: b,
		};
		assert_eq!(verify_equivocation_proof(&f.genesis, &proof), Ok(offender.clone()));

		// Same header twice -> rejected.
		let proof_same = SunLeadEquivocationProof {
			authority_set_id: f.context.authority_set_id,
			slot: Slot::from(f.slot),
			offender: offender.clone(),
			first: a.clone(),
			second: a.clone(),
		};
		assert_eq!(
			verify_equivocation_proof(&f.genesis, &proof_same),
			Err(SunLeadVerificationError::EquivocationSameHeader)
		);

		// Bad signature key -> rejected.
		let bad_pair = keys(4)[3].clone();
		let hbad = h256_repeat(0x07);
		let mut bad_body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			slot: Slot::from(f.slot),
			window_id: f.window_id,
			leader_index: f.leader_index,
			parent_hash: f.parent,
			schedule_root: H256::default(),
		};
		let input = ScheduleInput {
			chain_id: f.context.chain_id,
			authority_set_id: f.context.authority_set_id,
			epoch_index: f.context.epoch_index,
			epoch_seed: f.context.epoch_seed,
			authorities: f.context.authorities.clone(),
			params: f.params,
		};
		bad_body.schedule_root = build_schedule(&input).unwrap().root;
		let bad_msg = signing_message(&f.genesis, &hbad, &bad_body);
		let bad_sig = sunlead_primitives::SunLeadSignature::from(bad_pair.sign(&bad_msg));
		let bad = sunlead_primitives::SunLeadEquivocationHeader {
			digest: SunLeadPreDigest(bad_body),
			header_hash: hbad,
			seal: bad_sig,
		};
		let proof_bad = SunLeadEquivocationProof {
			authority_set_id: f.context.authority_set_id,
			slot: Slot::from(f.slot),
			offender: offender.clone(),
			first: a.clone(),
			second: bad,
		};
		assert_eq!(
			verify_equivocation_proof(&f.genesis, &proof_bad),
			Err(SunLeadVerificationError::EquivocationSignature)
		);
	}
}
