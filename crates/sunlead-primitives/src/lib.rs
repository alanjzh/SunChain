//! # sunlead-primitives
//!
//! Shared types for the **SunLead** deterministic-leader consensus engine.
//!
//! SunLead replaces BABE's per-slot VRF competition with a stake-weighted,
//! epoch-frozen *schedule*: every slot has exactly one canonical leader, chosen
//! deterministically from a finalized authority snapshot and an epoch seed.
//!
//! This crate contains **only pure data types, the `sunl` session-key crypto
//! wrapper and the signature/equivocation evidence shapes**. It has no knowledge
//! of block production, import queues or GRANDPA. It is `no_std` so it can be
//! compiled into the runtime WASM blob and reused by the node-side verifier.
//!
//! See `docs/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md` for the
//! protocol design. P1 (this crate + `sunlead-schedule` + `sunlead-verifier`)
//! is pure logic and cryptography; no consensus wiring happens here.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_application_crypto::{app_crypto, sr25519, KeyTypeId};
use sp_consensus_slots::Slot;
use sp_core::H256;
use sp_runtime::ConsensusEngineId;

/// The `sunl` session key type id ("sunl").
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"sunl");

/// The `ConsensusEngineId` written into block-header digests by SunLead.
pub const SUNLEAD_ENGINE_ID: ConsensusEngineId = *b"sunl";

/// Current SunLead protocol version. Must be bumped on any digest-format change.
pub const SUNLEAD_PROTOCOL_VERSION: u16 = 1;

app_crypto!(sr25519, KEY_TYPE);

/// A SunLead authority identifier (the `sunl` session public key).
pub type SunLeadId = Public;
/// A SunLead authority signature over a block header's pre-digest.
pub type SunLeadSignature = Signature;
/// A SunLead authority key pair (signing side; `std` only helpers).
pub type SunLeadPair = Pair;

/// Index of an epoch within a session/authority-set lifetime.
pub type EpochIndex = u64;
/// Identifier of the active authority set (mirrors GRANDPA/Grandpa set id).
pub type AuthoritySetId = u64;
/// Stake weight of an authority, used for schedule apportionment.
pub type Stake = u128;
/// Identifier of a contiguous run (window) of slots owned by one leader.
pub type WindowId = u32;

/// Governance ceiling on [`SunLeadScheduleParams::max_window_slots`].
///
/// A single leader may never own more than this many *consecutive* slots. This is
/// the primary fairness / liveness lever of the schedule: it bounds the worst-case
/// run length any one validator can monopolize, and therefore also bounds the
/// deviation [`sunlead_schedule::enforce_feasible`] may introduce from the pure
/// stake-proportional apportionment (see that function for the math). Kept far
/// above the V1 default (4) but small enough that no validator can own a long
/// contiguous stretch of the epoch.
pub const MAX_WINDOW_SLOTS: u32 = 64;

/// Upper bound on [`SunLeadScheduleParams::epoch_slots`]. A `u64` is technically
/// unbounded; this guards against accidental overflow downstream and against
/// absurdly long epochs that would never complete a snapshot rebuild.
pub const MAX_EPOCH_SLOTS: u64 = 4_194_304; // 2^22

/// Schedule parameters frozen per epoch.
///
/// `window_len` is the *target* run length for a leader; `max_window_slots` is
/// the hard cap on consecutive slots a single leader may own. The schedule
/// generator tries to keep runs at `window_len` and only extends a run up to
/// `max_window_slots` when forced (e.g. very skewed stake).
///
/// ## Deviation bound (why these values matter)
///
/// The per-authority slot quota is first apportioned stake-proportionally
/// (Hamilton / largest-remainder). When stake is skewed,
/// [`sunlead_schedule::enforce_feasible`] caps the largest authority's quota at
/// `max_window_slots * (others + 1)` so the run-length-bounded arrangement stays
/// feasible (classic task-scheduler condition, with `L = max_window_slots`).
/// Concretely:
///
/// * a single authority's *actual* share can deviate from its stake-proportional
///   share by **at most** `max_window_slots * (others + 1)` slots (the excess is
///   redistributed to — never taken from — the other authorities);
/// * no authority ever owns more than `max_window_slots` *consecutive* slots.
///
/// Keeping `max_window_slots` within [`MAX_WINDOW_SLOTS`] is what makes that
/// deviation (and any single run) finitely bounded and governance-acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct SunLeadScheduleParams {
	/// Number of slots in the epoch.
	pub epoch_slots: u64,
	/// Target consecutive slots per leader (V1: 2).
	pub window_len: u32,
	/// Hard cap on consecutive slots per leader (V1: 4).
	pub max_window_slots: u32,
}

/// Reasons a [`SunLeadScheduleParams`] is outside the governance-acceptable range.
///
/// Rejecting these combinations (at genesis or via governance) prevents a
/// misconfigured schedule from ever being built — an invalid combination would
/// otherwise surface only as a (fatal) infeasible-schedule panic deep in block
/// production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleParamsError {
	/// `epoch_slots` must be >= 1.
	ZeroEpochSlots,
	/// `epoch_slots` exceeds [`MAX_EPOCH_SLOTS`].
	EpochSlotsTooLarge,
	/// `window_len` must be >= 1.
	WindowLenZero,
	/// `max_window_slots` must be >= `window_len`.
	MaxBelowWindow,
	/// `max_window_slots` cannot exceed `epoch_slots` (a run can't be longer than
	/// the whole epoch).
	RunExceedsEpoch,
	/// `max_window_slots` exceeds the governance ceiling [`MAX_WINDOW_SLOTS`].
	WindowCapTooLarge,
}

impl SunLeadScheduleParams {
	/// V1 default parameters from the development plan: 600 slots/epoch,
	/// 2-slot window, 4-slot hard cap.
	pub const fn v1_defaults() -> Self {
		Self { epoch_slots: 600, window_len: 2, max_window_slots: 4 }
	}

	/// Validate against the governance-acceptable range. See the type docs for the
	/// rationale behind each bound (in particular the `enforce_feasible`
	/// deviation bound tied to `max_window_slots`).
	pub const fn validate(&self) -> Result<(), ScheduleParamsError> {
		if self.epoch_slots == 0 {
			return Err(ScheduleParamsError::ZeroEpochSlots);
		}
		if self.epoch_slots > MAX_EPOCH_SLOTS {
			return Err(ScheduleParamsError::EpochSlotsTooLarge);
		}
		if self.window_len == 0 {
			return Err(ScheduleParamsError::WindowLenZero);
		}
		if self.max_window_slots < self.window_len {
			return Err(ScheduleParamsError::MaxBelowWindow);
		}
		if self.max_window_slots as u64 > self.epoch_slots {
			return Err(ScheduleParamsError::RunExceedsEpoch);
		}
		if self.max_window_slots > MAX_WINDOW_SLOTS {
			return Err(ScheduleParamsError::WindowCapTooLarge);
		}
		Ok(())
	}
}

impl Default for SunLeadScheduleParams {
	fn default() -> Self {
		Self::v1_defaults()
	}
}

/// The body of a SunLead pre-digest, i.e. everything **except** the signature.
///
/// It is signed as-is (together with the engine id, chain genesis hash and the
/// block header hash) so every field below is cryptographically bound to the
/// block: replay across chain / epoch / version / parent is impossible.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadPreDigestBody {
	/// Protocol version.
	pub version: u16,
	/// Authority set the schedule was derived from.
	pub authority_set_id: AuthoritySetId,
	/// Epoch the block belongs to.
	pub epoch_index: EpochIndex,
	/// Slot of this block.
	pub slot: Slot,
	/// Window (run) id this slot belongs to.
	pub window_id: WindowId,
	/// Index of the scheduled leader within the canonical authority list.
	pub leader_index: u32,
	/// Hash of the parent block.
	pub parent_hash: H256,
	/// Merkle root committing to the whole epoch schedule.
	pub schedule_root: H256,
}

/// A SunLead block-header pre-digest (the **unsigned** body).
///
/// This is the `sunl` consensus item placed in a block header as a pre-runtime
/// digest. It must be present and valid (verified before runtime execution) for a
/// SunLead block to be imported. The leader's signature is carried *separately*
/// as a `DigestItem::Seal(SUNLEAD_ENGINE_ID, ..)` (see [`verify_seal`]) so it can
/// bind the pre-seal header hash — putting the signature inside this body would
/// be circular, since the pre-seal hash includes the pre-runtime log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadPreDigest(pub SunLeadPreDigestBody);

impl SunLeadPreDigest {
	/// The body half of the digest (everything except the signature).
	pub fn body(&self) -> &SunLeadPreDigestBody {
		&self.0
	}
}

/// One authority entry in a (canonically ordered) authority snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadAuthority<AccountId> {
	/// Staking account controlling the authority.
	pub account: AccountId,
	/// The `sunl` session key (leader identity).
	pub id: SunLeadId,
	/// Stake weight used for schedule apportionment.
	pub stake: Stake,
}

/// A frozen, epoch-scoped authority snapshot.
///
/// Produced by the runtime from finalized state at the epoch boundary and
/// consumed by the node to derive the deterministic schedule.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadAuthoritySnapshot<AccountId> {
	/// Chain id, bound into the schedule seed.
	pub chain_id: u64,
	/// Protocol version.
	pub protocol_version: u16,
	/// Authority set id.
	pub authority_set_id: AuthoritySetId,
	/// Epoch index.
	pub epoch_index: EpochIndex,
	/// First slot at which this schedule becomes active.
	pub activation_slot: Slot,
	/// Authorities in canonical `(account, id)` order.
	pub authorities: Vec<SunLeadAuthority<AccountId>>,
	/// Epoch seed from finalized historical randomness.
	pub epoch_seed: H256,
	/// Merkle root of the derived schedule.
	pub schedule_root: H256,
	/// Schedule parameters frozen at this epoch boundary.
	///
	/// Verification must use these frozen values rather than the live, mutable
	/// `ScheduleParams` storage: freezing guarantees every block in an epoch is
	/// checked against the exact parameters that produced its `schedule_root`,
	/// so a mid-epoch governance change to `ScheduleParams` cannot retroactively
	/// invalidate (or be exploited to forge) already-signed blocks.
	pub params: SunLeadScheduleParams,
}

/// A minimal context sufficient to *derive and verify* a schedule, without the
/// `AccountId` storage boilerplate of [`SunLeadAuthoritySnapshot`].
///
/// The verifier works against this; the runtime builds it from the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadScheduleContext {
	/// Chain id.
	pub chain_id: u64,
	/// Authority set id.
	pub authority_set_id: AuthoritySetId,
	/// Epoch index.
	pub epoch_index: EpochIndex,
	/// Epoch seed.
	pub epoch_seed: H256,
	/// Authorities in canonical `(account, id)` order: `(id, stake)`.
	pub authorities: Vec<(SunLeadId, Stake)>,
}

impl<AccountId: Clone> From<&SunLeadAuthoritySnapshot<AccountId>> for SunLeadScheduleContext {
	fn from(s: &SunLeadAuthoritySnapshot<AccountId>) -> Self {
		Self {
			chain_id: s.chain_id,
			authority_set_id: s.authority_set_id,
			epoch_index: s.epoch_index,
			epoch_seed: s.epoch_seed,
			authorities: s.authorities.iter().map(|a| (a.id.clone(), a.stake)).collect(),
		}
	}
}

/// One header in an equivocation proof: the unsigned pre-digest, the pre-seal
/// header hash it was sealed over, and the leader's seal.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadEquivocationHeader {
	/// Unsigned pre-digest of the header.
	pub digest: SunLeadPreDigest,
	/// Pre-seal header hash that the seal signs over.
	pub header_hash: H256,
	/// Leader's seal (signature over the pre-seal hash + body).
	pub seal: SunLeadSignature,
}

/// An equivocation (double-signing) proof.
///
/// Two distinct, valid SunLead headers for the same `(authority_set_id, slot,
/// leader_index)` constitute an offence. The `offender` key is carried so the
/// evidence is self-verifying; the runtime (P2) additionally checks that
/// `offender` is indeed the leader at `leader_index`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadEquivocationProof {
	/// Authority set id shared by both headers.
	pub authority_set_id: AuthoritySetId,
	/// Slot shared by both headers.
	pub slot: Slot,
	/// The offending leader's public key.
	pub offender: SunLeadId,
	/// First header evidence.
	pub first: SunLeadEquivocationHeader,
	/// Second header evidence.
	pub second: SunLeadEquivocationHeader,
}

/// Sort authorities into canonical `(account, id)` order.
///
/// Determinism of the schedule depends on every node ordering authorities
/// identically, so this is the single canonical ordering used everywhere.
pub fn canonical_authorities<AccountId: Clone + Ord>(
	mut auths: Vec<SunLeadAuthority<AccountId>>,
) -> Vec<SunLeadAuthority<AccountId>> {
	auths.sort_by(|a, b| {
		a.account
			.cmp(&b.account)
			.then_with(|| {
				<SunLeadId as AsRef<[u8]>>::as_ref(&a.id)
					.cmp(<SunLeadId as AsRef<[u8]>>::as_ref(&b.id))
			})
	});
	auths
}

/// Build the signing message bound into every SunLead signature.
///
/// The message binds: the engine id (domain separation), the chain genesis hash
/// (cross-chain replay protection), the block header hash (cross-block replay
/// protection) and the full digest body (epoch / set / slot / parent / leader /
/// schedule root).
pub fn signing_message(
	genesis_hash: &H256,
	header_hash: &H256,
	body: &SunLeadPreDigestBody,
) -> Vec<u8> {
	let mut m = Vec::new();
	SUNLEAD_ENGINE_ID.encode_to(&mut m);
	genesis_hash.encode_to(&mut m);
	header_hash.encode_to(&mut m);
	body.encode_to(&mut m);
	m
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::crypto::Pair as _;
	use sp_core::sr25519::{Pair, Signature};

	fn h256_repeat(b: u8) -> H256 {
		H256::from_slice(&[b; 32])
	}

	#[test]
	fn signing_payload_is_stable_and_binds_all_fields() {
		let genesis = h256_repeat(0xaa);
		let header = h256_repeat(0xbb);
		let body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: 7,
			epoch_index: 3,
			slot: Slot::from(42u64),
			window_id: 5,
			leader_index: 1,
			parent_hash: h256_repeat(0xcc),
			schedule_root: h256_repeat(0xdd),
		};
		let msg = signing_message(&genesis, &header, &body);
		// Determinism.
		assert_eq!(msg, signing_message(&genesis, &header, &body));
		// Any field change changes the message.
		let mut body2 = body.clone();
		body2.slot = Slot::from(43u64);
		assert_ne!(msg, signing_message(&genesis, &header, &body2));
		let mut header2 = header;
		header2 = h256_repeat(0xee);
		assert_ne!(msg, signing_message(&genesis, &header2, &body));
	}

	#[test]
	fn signature_roundtrips_and_replay_is_rejected() {
		let pair = Pair::from_seed(&[1u8; 32]);
		let pubkey = SunLeadId::from(pair.public());
		let genesis = h256_repeat(0x01);
		let header = h256_repeat(0x02);
		let body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: 1,
			epoch_index: 1,
			slot: Slot::from(10u64),
			window_id: 0,
			leader_index: 0,
			parent_hash: h256_repeat(0x03),
			schedule_root: h256_repeat(0x04),
		};
		let msg = signing_message(&genesis, &header, &body);
		let sig = SunLeadSignature::from(pair.sign(&msg));
		// Verify via the inner sr25519 key (avoids trait-in-scope juggling).
		let inner_pub: sp_core::sr25519::Public = pubkey.into();
		let inner_sig: sp_core::sr25519::Signature = sig.clone().into();
		assert!(sp_core::sr25519::Pair::verify(&inner_sig, &msg, &inner_pub));

		// Different header -> different message -> signature must not verify.
		let other_header = h256_repeat(0xff);
		let other_msg = signing_message(&genesis, &other_header, &body);
		assert!(!sp_core::sr25519::Pair::verify(&inner_sig, &other_msg, &inner_pub));

		// Wrong key.
		let other_pair = Pair::from_seed(&[2u8; 32]);
		let other_pub: sp_core::sr25519::Public = SunLeadId::from(other_pair.public()).into();
		assert!(!sp_core::sr25519::Pair::verify(&inner_sig, &msg, &other_pub));

		// A raw sr25519 signature is structurally a SunLeadSignature.
		let _: Signature = sig.into();
	}

	#[test]
	fn schedule_params_validate_accepts_defaults_and_bounds() {
		assert!(SunLeadScheduleParams::v1_defaults().validate().is_ok());
		// A tighter but still valid configuration passes.
		let tight = SunLeadScheduleParams { epoch_slots: 600, window_len: 1, max_window_slots: 1 };
		assert!(tight.validate().is_ok());
		// The governance ceiling itself is allowed.
		let at_ceiling =
			SunLeadScheduleParams { epoch_slots: 600, window_len: 2, max_window_slots: MAX_WINDOW_SLOTS };
		assert!(at_ceiling.validate().is_ok());
	}

	#[test]
	fn schedule_params_validate_rejects_each_bound() {
		// ZeroEpochSlots
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.epoch_slots = 0;
		assert_eq!(p.validate(), Err(ScheduleParamsError::ZeroEpochSlots));

		// EpochSlotsTooLarge
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.epoch_slots = MAX_EPOCH_SLOTS + 1;
		assert_eq!(p.validate(), Err(ScheduleParamsError::EpochSlotsTooLarge));

		// WindowLenZero
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.window_len = 0;
		assert_eq!(p.validate(), Err(ScheduleParamsError::WindowLenZero));

		// MaxBelowWindow (window_len > max_window_slots)
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.window_len = 8;
		p.max_window_slots = 4;
		assert_eq!(p.validate(), Err(ScheduleParamsError::MaxBelowWindow));

		// RunExceedsEpoch (max_window_slots > epoch_slots)
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.max_window_slots = 601;
		assert_eq!(p.validate(), Err(ScheduleParamsError::RunExceedsEpoch));

		// WindowCapTooLarge (above the governance ceiling, but within the epoch)
		let mut p = SunLeadScheduleParams::v1_defaults();
		p.max_window_slots = MAX_WINDOW_SLOTS + 1;
		assert_eq!(p.validate(), Err(ScheduleParamsError::WindowCapTooLarge));
	}
}
