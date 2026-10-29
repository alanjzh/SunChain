//! SunLead import queue + verifier.
//!
//! The verifier runs *before* the runtime executes the block and enforces the
//! acceptance criteria: the only authority allowed to author `slot` is the
//! deterministically scheduled leader; the `schedule_root`, signature, parent
//! hash and slot monotonicity must all be valid. Anything else is rejected
//! before runtime execution.

use log::debug;
use prometheus_endpoint::Registry;
use sc_consensus::import_queue::BasicQueue;
use sc_consensus::{BlockImport as BlockImportT, BlockImportParams, Verifier};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_consensus::Error as ConsensusError;
use sp_consensus_slots::Slot;
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};
use sp_runtime::DigestItem;
use scale_codec::Encode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use sunchain_runtime::apis::SunLeadApi;
use sunchain_runtime::opaque::Block;
use sunlead_primitives::{
	SunLeadPreDigest, SunLeadScheduleContext, SunLeadScheduleParams, SunLeadSignature,
	SUNLEAD_ENGINE_ID,
};
use sunlead_verifier::{
	verify_parent_hash, verify_pre_digest, verify_seal, verify_slot_monotonicity,
};

use crate::sunlead_consensus::block_import::SunLeadBlockImport;
use crate::sunlead_consensus::metrics::{reason_from_error, SunLeadMetrics};

/// The hash a leader signs over: the block header *with* the SunLead seal
/// removed from its digest (the pre-seal hash).
///
/// The seal lives in a `DigestItem::Seal`, so stripping it and re-hashing yields
/// exactly the header hash the leader bound its signature to.
pub fn pre_seal_hash<H: HeaderT>(header: &H) -> H256 {
	let mut header = header.clone();
	header.digest_mut().logs.retain(
		|log| log.seal_try_to::<SunLeadSignature>(&SUNLEAD_ENGINE_ID).is_none(),
	);
	H256::from_slice(header.hash().as_ref())
}

/// Verifier that checks the SunLead pre-digest attached to every SunLead-authored
/// block.
pub struct SunLeadVerifier<C> {
	client: Arc<C>,
	genesis_hash: H256,
	/// Node-side verification metrics. `None` in tests that do not wire a registry.
	metrics: Option<Arc<SunLeadMetrics>>,
	/// Best seal seen per slot, for equivocation detection. Two different seals at
	/// the same slot mean two different (conflicting) authored blocks — an
	/// equivocation attempt. Re-imports of the same block reuse the same seal and
	/// are not double-counted. Bounded by periodic clearing.
	slot_seals: Arc<Mutex<HashMap<Slot, Vec<u8>>>>,
}

impl<C> SunLeadVerifier<C> {
	/// Build a verifier bound to `client` and the chain `genesis_hash` (bound into
	/// the digest signature for cross-chain replay protection).
	///
	/// `metrics` is optional so tests can construct a verifier without a registry.
	pub fn new(
		client: Arc<C>,
		genesis_hash: H256,
		metrics: Option<Arc<SunLeadMetrics>>,
	) -> Self {
		Self { client, genesis_hash, metrics, slot_seals: Arc::new(Mutex::new(HashMap::new())) }
	}
}

/// Locate the single SunLead pre-runtime digest in a header.
///
/// Rejects a missing digest, a duplicate digest, and a digest that claims the
/// SunLead engine id but fails to decode (an undecodable SunLead item must not be
/// silently skipped, or an attacker could smuggle a malformed header past the
/// verifier).
pub fn find_sunlead_pre_digest<H: HeaderT>(header: &H) -> Result<SunLeadPreDigest, ConsensusError> {
	let mut found: Option<SunLeadPreDigest> = None;
	for log in header.digest().logs() {
		if log.as_pre_runtime().map(|(id, _)| id) == Some(SUNLEAD_ENGINE_ID) {
			let d = log
				.pre_runtime_try_to::<SunLeadPreDigest>(&SUNLEAD_ENGINE_ID)
				.ok_or_else(|| {
					ConsensusError::ClientImport(
						"sunlead pre-runtime digest present but undecodable".into(),
					)
				})?;
			if found.is_some() {
				return Err(ConsensusError::ClientImport(
					"multiple sunlead pre-runtime digests in header".into(),
				));
			}
			found = Some(d);
		}
	}
	found.ok_or_else(|| ConsensusError::ClientImport("no sunlead pre-runtime digest found".into()))
}

/// Extract the single SunLead seal (`DigestItem::Seal(SUNLEAD_ENGINE_ID, ..)`) from
/// a header, rejecting a missing, duplicate, or undecodable seal.
pub fn find_sunlead_seal<H: HeaderT>(header: &H) -> Result<SunLeadSignature, ConsensusError> {
	let mut found: Option<SunLeadSignature> = None;
	for log in header.digest().logs() {
		if log.as_seal().map(|(id, _)| id) == Some(SUNLEAD_ENGINE_ID) {
			let s = log
				.seal_try_to::<SunLeadSignature>(&SUNLEAD_ENGINE_ID)
				.ok_or_else(|| {
					ConsensusError::ClientImport("sunlead seal present but undecodable".into())
				})?;
			if found.is_some() {
				return Err(ConsensusError::ClientImport(
					"multiple sunlead seals in header".into(),
				));
			}
			found = Some(s);
		}
	}
	found.ok_or_else(|| ConsensusError::ClientImport("no sunlead seal found".into()))
}

/// Reject any pre-runtime digest or seal that belongs to a *different* consensus
/// engine (e.g. BABE) on a SunLead-authored header. A SunLead-active chain must
/// not also carry a foreign engine's authoring items, which would let an attacker
/// mix consensus protocols or smuggle a fallback authoring path.
pub fn check_no_conflicting_engine_items<H: HeaderT>(header: &H) -> Result<(), ConsensusError> {
	for log in header.digest().logs() {
		if let Some((id, _)) = log.as_pre_runtime() {
			if id != SUNLEAD_ENGINE_ID {
				return Err(ConsensusError::ClientImport(format!(
					"unexpected foreign pre-runtime digest for engine {:?}",
					id
				)));
			}
		}
		if let Some((id, _)) = log.as_seal() {
			if id != SUNLEAD_ENGINE_ID {
				return Err(ConsensusError::ClientImport(format!(
					"unexpected foreign seal for engine {:?}",
					id
				)));
			}
		}
	}
	Ok(())
}

/// Locate the slot of a SunLead pre-digest in a header, if present.
pub fn find_sunlead_slot<H: HeaderT>(header: &H) -> Option<Slot> {
	header.digest().logs().iter().find_map(|log| {
		log.pre_runtime_try_to::<SunLeadPreDigest>(&SUNLEAD_ENGINE_ID).map(|d| d.body().slot)
	})
}

/// Resolved on-chain inputs needed to verify a SunLead header.
///
/// The remote import verifier derives these from the client (snapshot + config +
/// parent header); the local authoring self-check already holds the equivalent
/// `aux_data`; tests construct them directly. Keeping the inputs explicit lets
/// the *same* pure verification be applied in all three contexts, so a block
/// authored locally is checked with exactly the logic a peer applies to a
/// gossiped block.
#[derive(Debug)]
pub struct SunLeadHeaderInputs {
	/// Verification context (authorities, set id, epoch, seed) for the schedule.
	pub context: SunLeadScheduleContext,
	/// Schedule parameters for the epoch.
	pub params: SunLeadScheduleParams,
	/// Slot of the parent's SunLead pre-digest, if the parent is itself a SunLead
	/// header. `None` means slot monotonicity is not checked (e.g. a genesis
	/// parent, which carries no SunLead pre-digest to compare against).
	pub parent_sunlead_slot: Option<Slot>,
}

/// Output of a successful [`verify_sunlead_header`].
#[derive(Debug)]
pub struct VerifiedSunLeadHeader {
	/// The decoded pre-digest.
	pub pre_digest: SunLeadPreDigest,
	/// The decoded seal.
	pub seal: SunLeadSignature,
	/// The pre-seal header hash the seal binds to. Retained for callers that build
	/// equivocation proofs; not read by the import verifier itself.
	#[allow(dead_code)]
	pub pre_seal_hash: H256,
}

/// Shared SunLead header verification, used by the remote import verifier *and*
/// the local authoring self-check.
///
/// Performs the full pre-import acceptance checklist: exactly one SunLead
/// pre-digest and one seal (rejecting duplicates and undecodable items), no
/// conflicting foreign-engine items, then the pure cryptographic / schedule
/// checks (`verify_pre_digest`, `verify_seal`, `verify_parent_hash`, and
/// `verify_slot_monotonicity`). The genesis block carries no SunLead pre-digest
/// and must be handled by the caller (it is passed through untouched).
pub fn verify_sunlead_header<B: BlockT>(
	header: &B::Header,
	genesis_hash: H256,
	inputs: &SunLeadHeaderInputs,
) -> Result<VerifiedSunLeadHeader, String> {
	let pre_digest =
		find_sunlead_pre_digest(header).map_err(|e| e.to_string())?;
	// A SunLead-authored header must carry no foreign consensus engine's
	// pre-runtime digest or seal (e.g. BABE). Reject any conflicting item so a
	// block cannot mix consensus protocols.
	check_no_conflicting_engine_items(header).map_err(|e| e.to_string())?;
	let pre_seal_hash = pre_seal_hash(header);

	verify_pre_digest(&genesis_hash, &pre_seal_hash, &pre_digest, &inputs.context, &inputs.params)
		.map_err(|e| format!("sunlead: pre-digest verification failed: {:?}", e))?;

	// The leader's seal signs the pre-seal header hash; it lives in the
	// post-digest seal, not inside the pre-digest body (to avoid a circular
	// dependency on the hash it signs).
	let seal = find_sunlead_seal(header).map_err(|e| e.to_string())?;
	verify_seal(genesis_hash, pre_seal_hash, &pre_digest, &inputs.context, &seal)
		.map_err(|e| format!("sunlead: seal verification failed: {:?}", e))?;

	verify_parent_hash(
		pre_digest.body().parent_hash,
		H256::from_slice(header.parent_hash().as_ref()),
	)
	.map_err(|e| format!("sunlead: parent hash mismatch: {:?}", e))?;

	// Slot monotonicity: the parent's slot must be strictly less than this slot.
	// A parent header that cannot be fetched (or is missing) is a *hard
	// failure*, not something we silently skip — skipping would let a
	// withheld or reorganised parent bypass the monotonicity invariant. The
	// only acceptable exception is a non-SunLead parent (e.g. genesis), which
	// carries no SunLead pre-digest to compare against.
	if let Some(parent_slot) = inputs.parent_sunlead_slot {
		verify_slot_monotonicity(parent_slot, pre_digest.body().slot)
			.map_err(|e| format!("sunlead: slot monotonicity violated: {:?}", e))?;
	}

	Ok(VerifiedSunLeadHeader { pre_digest, seal, pre_seal_hash })
}

#[async_trait::async_trait]
impl<C> Verifier<Block> for SunLeadVerifier<C>
where
	C: ProvideRuntimeApi<Block> + HeaderBackend<Block> + Send + Sync + 'static,
	C::Api: SunLeadApi<Block>,
{
	async fn verify(
		&self,
		mut block: BlockImportParams<Block>,
	) -> Result<BlockImportParams<Block>, String> {
		let header = block.header.clone();
		// The genesis block carries no SunLead pre-digest (it is not authored by the
		// schedule). Pass it through untouched so the underlying import can finalize it.
		if *header.number() == 0 {
			return Ok(block);
		}
		let parent_hash = *header.parent_hash();

		let api = self.client.runtime_api();
		let snapshot = api
			.current_snapshot(parent_hash)
			.map_err(|e| format!("sunlead: failed to fetch snapshot: {:?}", e))?
			.ok_or_else(|| "sunlead: schedule snapshot not available".to_string())?;

		// Freeze the schedule parameters at the epoch boundary: verify against the
		// snapshot's frozen `params`, never the live, mutable `ScheduleParams`
		// storage, so a mid-epoch governance change cannot retroactively invalidate
		// or forge already-signed blocks.
		let params: SunLeadScheduleParams = snapshot.params;

		// Reconstruct the verification context from the on-chain snapshot.
		let context: SunLeadScheduleContext = (&snapshot).into();

		// Resolve the parent's SunLead slot (for monotonicity); a non-SunLead
		// parent (e.g. genesis) yields `None` and monotonicity is skipped.
		let parent_sunlead_slot = self
			.client
			.header(parent_hash)
			.map_err(|e| format!("sunlead: failed to fetch parent header: {:?}", e))?
			.ok_or_else(|| "sunlead: parent header not found".to_string())
			.map(|h| find_sunlead_slot(&h))?;

		let inputs = SunLeadHeaderInputs { context, params, parent_sunlead_slot };
		let verified = match verify_sunlead_header::<Block>(&header, self.genesis_hash, &inputs) {
			Ok(v) => v,
			Err(e) => {
				if let Some(m) = &self.metrics {
					m.record_rejected(reason_from_error(&e));
				}
				return Err(e);
			}
		};

		let seal = verified.seal;
		let slot = verified.pre_digest.body().slot;

		// Equivocation: a different seal already seen for this slot means a second,
		// conflicting authored block at the same slot — count it. Re-imports of the
		// same block reuse the same seal and are not double-counted.
		if let Some(m) = &self.metrics {
			let mut map = self.slot_seals.lock().unwrap_or_else(|e| e.into_inner());
			let encoded = seal.encode();
			match map.get(&slot) {
				Some(prev) if prev.as_slice() != encoded.as_slice() => {
					m.record_equivocation();
				}
				_ => {
					map.insert(slot, encoded);
				}
			}
			if map.len() > 8192 {
				map.clear();
			}
		}

		// Move the SunLead seal out of the header's digest and into `post_digests`.
		//
		// When a remote (gossiped) block is imported, the runtime re-executes the
		// block from the *pre-seal* header and re-deposits the runtime-driven logs
		// (in particular the Frontier `fron` PostLog written by
		// `pallet_ethereum::store_block`). `Executive::final_checks` then asserts
		// the header's digest equals the digest the runtime computed. The recomputed
		// digest only carries the PreRuntime item (the seal is stripped by
		// `Executive::extract_pre_digest`) plus the runtime-deposited logs — so it
		// has one fewer item than the full header, which still carries the Seal. The
		// mismatch (`left: 3, right: 2`) panics and prevents the block from being
		// imported, which in turn starves GRANDPA.
		//
		// The BABE verifier does exactly this relocation (`block.header` loses the
		// seal, `block.post_digests` gains it) so that the runtime sees a pre-seal
		// header whose digest matches its own recomputation. We mirror it here.
		// Local authoring never reaches this verifier and already places the seal in
		// `post_digests`, so this branch is a no-op for own blocks.
		block.header.digest_mut().logs.retain(
			|log| log.seal_try_to::<sunlead_primitives::SunLeadSignature>(&SUNLEAD_ENGINE_ID).is_none(),
		);
		block.post_digests.push(DigestItem::Seal(SUNLEAD_ENGINE_ID, seal.encode()));

		if let Some(m) = &self.metrics {
			m.record_verified();
		}
		debug!(target: "sunlead", "verified SunLead pre-digest for slot {}", slot);
		Ok(block)
	}
}

/// Build a SunLead import queue. `justification_import` is the chain's GRANDPA
/// block import, used to import justifications alongside the SunLead blocks.
pub fn sunlead_import_queue<C, I>(
	block_import: SunLeadBlockImport<Block, I>,
	client: Arc<C>,
	justification_import: Option<Box<dyn sc_consensus::JustificationImport<Block, Error = ConsensusError> + Send + Sync>>,
	genesis_hash: H256,
	spawner: &impl sp_core::traits::SpawnEssentialNamed,
	registry: Option<&Registry>,
) -> BasicQueue<Block>
where
	C: ProvideRuntimeApi<Block> + HeaderBackend<Block> + Send + Sync + 'static,
	C::Api: SunLeadApi<Block>,
	I: BlockImportT<Block> + Send + Sync + 'static,
	I::Error: Into<ConsensusError>,
{
	let metrics = Arc::new(SunLeadMetrics::new(registry));
	let verifier = SunLeadVerifier::new(client, genesis_hash, Some(metrics));
	BasicQueue::new(verifier, Box::new(block_import), justification_import, spawner, registry)
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_api::{ApiRef, ProvideRuntimeApi};
	use sp_blockchain::{BlockStatus, HeaderBackend, Info};
	use sp_consensus_slots::Slot;
	use sp_core::crypto::Pair as CryptoPair;
	use sp_core::sr25519::Pair;
	use sp_runtime::generic::Digest as GenericDigest;
	use sp_runtime::traits::NumberFor;
	use std::sync::Arc;
	use sc_consensus::{BlockImportParams, Verifier};
	use sp_consensus::BlockOrigin;
	use sunlead_primitives::{
		signing_message, EpochIndex, Stake, SunLeadAuthority, SunLeadAuthoritySnapshot, SunLeadId,
		SunLeadPreDigestBody, WindowId, SUNLEAD_PROTOCOL_VERSION,
	};
	use sunlead_schedule::{build_schedule, ScheduleInput, SunLeadScheduleProof};
	use sunchain_runtime::apis::SunLeadApi;
	use sunchain_runtime::{AccountId, opaque::{Block, Header as OpaqueHeader}};

	type H = OpaqueHeader;

	fn h256(b: u8) -> H256 {
		H256::repeat_byte(b)
	}

	/// Build a SunLead header (number 1) carrying exactly one pre-digest and one
	/// seal, signing the body with `keys[signer_index]`. `tamper` may mutate the
	/// pre-digest body before it is sealed, which lets each test isolate a single
	/// acceptance criterion. The schedule (and therefore the expected leader /
	/// `schedule_root` / `window_id`) is derived from `keys`/`stakes`/`params`, so
	/// the produced body is internally consistent unless `tamper` breaks it.
	fn build_header(
		keys: &[Pair],
		stakes: &[Stake],
		params: &SunLeadScheduleParams,
		genesis_hash: H256,
		parent_hash: H256,
		slot: Slot,
		signer_index: usize,
		tamper: impl Fn(&mut SunLeadPreDigestBody),
	) -> (H, SunLeadPreDigest) {
		let authorities: Vec<(SunLeadId, Stake)> = keys
			.iter()
			.zip(stakes.iter())
			.map(|(k, s)| (SunLeadId::from(k.public()), *s))
			.collect();
		let epoch_index = (*slot / params.epoch_slots) as EpochIndex;
		let context = SunLeadScheduleContext {
			chain_id: 1,
			authority_set_id: 1,
			epoch_index,
			epoch_seed: h256(7),
			authorities: authorities.clone(),
		};
		let input = ScheduleInput {
			chain_id: 1,
			authority_set_id: 1,
			epoch_index,
			epoch_seed: h256(7),
			authorities: authorities.clone(),
			params: *params,
		};
		let sched = build_schedule(&input).unwrap();
		let (leader_index, window_id) = sched.leader_at(*slot % params.epoch_slots).unwrap();
		let mut body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: 1,
			epoch_index,
			slot,
			window_id,
			leader_index,
			parent_hash,
			schedule_root: sched.root,
		};
		tamper(&mut body);
		let pre = SunLeadPreDigest(body.clone());

		let pre_only_digest = GenericDigest {
			logs: vec![DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode())],
		};
		let h0 = OpaqueHeader::new(1u32, h256(0), h256(0), parent_hash, pre_only_digest);
		let pre_seal = pre_seal_hash(&h0);
		let msg = signing_message(&genesis_hash, &pre_seal, &body);
		let sig = keys[signer_index].sign(&msg);
		let seal = SunLeadSignature::from(sig);

		let full_digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
				DigestItem::Seal(SUNLEAD_ENGINE_ID, seal.encode()),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent_hash, full_digest);
		(h, pre)
	}

	/// Build the verification inputs that mirror what the remote import verifier
	/// derives from the parent-state snapshot (same authorities / params / epoch).
	fn build_inputs(
		keys: &[Pair],
		stakes: &[Stake],
		params: &SunLeadScheduleParams,
		slot: Slot,
		parent_sunlead_slot: Option<Slot>,
	) -> SunLeadHeaderInputs {
		let authorities: Vec<(SunLeadId, Stake)> = keys
			.iter()
			.zip(stakes.iter())
			.map(|(k, s)| (SunLeadId::from(k.public()), *s))
			.collect();
		let context = SunLeadScheduleContext {
			chain_id: 1,
			authority_set_id: 1,
			epoch_index: (*slot / params.epoch_slots) as EpochIndex,
			epoch_seed: h256(7),
			authorities,
		};
		SunLeadHeaderInputs {
			context,
			params: *params,
			parent_sunlead_slot,
		}
	}

	fn fixture_keys(n: usize) -> Vec<Pair> {
		(0..n).map(|i| Pair::from_seed(&[i as u8; 32])).collect()
	}

	fn fixtures() -> (Vec<Pair>, Vec<Stake>, SunLeadScheduleParams, H256) {
		let keys = fixture_keys(4);
		let stakes = vec![100u128, 50, 30, 20];
		let params = SunLeadScheduleParams::v1_defaults();
		let genesis = h256(0xaa);
		(keys, stakes, params, genesis)
	}

	#[test]
	fn valid_header_verifies() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		assert!(verify_sunlead_header::<Block>(&h, genesis, &inputs).is_ok());
	}

	#[test]
	fn wrong_signature_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		// Sign with a non-leader key (index 3) while the body still names the
		// scheduled leader (index 0) -> seal must fail verification.
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 3, |_| {});
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("seal verification failed"), "got: {}", err);
	}

	#[test]
	fn wrong_schedule_root_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |b| {
			b.schedule_root = h256(0xff);
		});
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("pre-digest verification failed"), "got: {}", err);
	}

	#[test]
	fn wrong_leader_index_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		// Tamper the claimed leader to index 1 while keeping the signature from the
		// real leader (index 0). Pre-digest check fires first -> LeaderMismatch.
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |b| {
			b.leader_index = if b.leader_index == 0 { 1 } else { 0 };
		});
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("pre-digest verification failed"), "got: {}", err);
	}

	#[test]
	fn wrong_parent_hash_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |b| {
			b.parent_hash = h256(0x99);
		});
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("parent hash mismatch"), "got: {}", err);
	}

	#[test]
	fn slot_monotonicity_violation_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		// Parent slot equal to this slot -> not strictly greater.
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("slot monotonicity violated"), "got: {}", err);
	}

	#[test]
	fn duplicate_pre_digest_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (_, pre) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		// Two identical SunLead pre-runtime digests in the same header.
		let digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent, digest);
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("multiple sunlead pre-runtime digests"), "got: {}", err);
	}

	#[test]
	fn duplicate_seal_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (_, pre) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let seal = vec![0u8; 64];
		let digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
				DigestItem::Seal(SUNLEAD_ENGINE_ID, seal.clone()),
				DigestItem::Seal(SUNLEAD_ENGINE_ID, seal),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent, digest);
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("multiple sunlead seals"), "got: {}", err);
	}

	#[test]
	fn undecodable_pre_digest_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, vec![0xde, 0xad, 0xbe, 0xef]),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent, digest);
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("present but undecodable"), "got: {}", err);
	}

	#[test]
	fn undecodable_seal_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (_, pre) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
				DigestItem::Seal(SUNLEAD_ENGINE_ID, vec![0xde, 0xad, 0xbe, 0xef]),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent, digest);
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("present but undecodable"), "got: {}", err);
	}

	#[test]
	fn foreign_engine_item_is_rejected() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (_, pre) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		// A foreign (BABE) pre-runtime digest alongside the SunLead one.
		let digest = GenericDigest {
			logs: vec![
				DigestItem::PreRuntime(*b"babe", vec![1, 2, 3]),
				DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, pre.encode()),
			],
		};
		let h = OpaqueHeader::new(1u32, h256(0), h256(0), parent, digest);
		let inputs = build_inputs(&keys, &stakes, &params, slot, Some(slot - Slot::from(1u64)));
		let err = verify_sunlead_header::<Block>(&h, genesis, &inputs).unwrap_err();
		assert!(err.contains("unexpected foreign pre-runtime digest"), "got: {}", err);
	}

	// ------------------------------------------------------------------------
	// Mock client exercising the `SunLeadVerifier` adapter (client fetch + verify).
	// This covers the cases that depend on the runtime API / header backend rather
	// than the pure verification core already tested above.
	// ------------------------------------------------------------------------

	#[derive(Clone)]
	struct TestApi {
		genesis: H256,
		config: SunLeadScheduleParams,
		snapshot: Option<SunLeadAuthoritySnapshot<AccountId>>,
		parent_header: Option<OpaqueHeader>,
	}

	impl ProvideRuntimeApi<Block> for TestApi {
		type Api = RuntimeApi;
		fn runtime_api(&self) -> ApiRef<RuntimeApi> {
			RuntimeApi { inner: self.clone() }.into()
		}
	}

	pub struct RuntimeApi {
		inner: TestApi,
	}

	sp_api::mock_impl_runtime_apis! {
		impl SunLeadApi<Block> for RuntimeApi {
			fn configuration() -> SunLeadScheduleParams { self.inner.config }
			fn current_snapshot() -> Option<SunLeadAuthoritySnapshot<AccountId>> {
				self.inner.snapshot.clone()
			}
			fn leader_at(_slot: u64) -> Option<(u32, WindowId)> { None }
			fn schedule_proof(_slot: u64) -> Option<SunLeadScheduleProof> { None }
			fn is_active() -> bool { true }
		}
	}

	impl HeaderBackend<Block> for TestApi {
		fn header(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> Result<Option<OpaqueHeader>, sp_blockchain::Error> {
			Ok(self.parent_header.clone())
		}
		fn info(&self) -> Info<Block> {
			Info {
				best_hash: self.genesis,
				best_number: 0,
				genesis_hash: self.genesis,
				finalized_hash: self.genesis,
				finalized_number: 0,
				finalized_state: None,
				number_leaves: 0,
				block_gap: None,
			}
		}
		fn status(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> Result<BlockStatus, sp_blockchain::Error> {
			Ok(BlockStatus::InChain)
		}
		fn number(
			&self,
			_hash: <Block as BlockT>::Hash,
		) -> Result<Option<NumberFor<Block>>, sp_blockchain::Error> {
			Ok(Some(0))
		}
		fn hash(
			&self,
			_number: NumberFor<Block>,
		) -> Result<Option<<Block as BlockT>::Hash>, sp_blockchain::Error> {
			Ok(None)
		}
	}

	fn build_snapshot(
		keys: &[Pair],
		stakes: &[Stake],
		params: &SunLeadScheduleParams,
		slot: Slot,
	) -> SunLeadAuthoritySnapshot<AccountId> {
		let epoch_index = (*slot / params.epoch_slots) as EpochIndex;
		SunLeadAuthoritySnapshot {
			chain_id: 1,
			protocol_version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: 1,
			epoch_index,
			activation_slot: slot,
			authorities: keys
				.iter()
				.zip(stakes.iter())
				.map(|(k, s)| SunLeadAuthority {
					account: AccountId::from([0u8; 20]),
					id: SunLeadId::from(k.public()),
					stake: *s,
				})
				.collect(),
			epoch_seed: h256(7),
			schedule_root: h256(0),
			params: *params,
		}
	}

	fn run_verify(
		api: TestApi,
		header: OpaqueHeader,
		genesis: H256,
	) -> Result<BlockImportParams<Block>, String> {
		let verifier = SunLeadVerifier::new(Arc::new(api), genesis, None);
		let bip = BlockImportParams::new(BlockOrigin::Own, header);
		let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
		rt.block_on(verifier.verify(bip))
	}

	#[test]
	fn verifier_happy_path_through_adapter() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let snapshot = build_snapshot(&keys, &stakes, &params, slot);
		// Non-SunLead parent header => slot monotonicity is skipped (parent has no
		// SunLead pre-digest), which is exactly what the adapter does for a genesis
		// parent.
		let parent_header =
			OpaqueHeader::new(0u32, h256(0), h256(0), h256(0), GenericDigest { logs: vec![] });
		let api = TestApi {
			genesis,
			config: params,
			snapshot: Some(snapshot),
			parent_header: Some(parent_header),
		};
		assert!(run_verify(api, h, genesis).is_ok());
	}

	#[test]
	fn verifier_no_snapshot_errors() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let parent_header =
			OpaqueHeader::new(0u32, h256(0), h256(0), h256(0), GenericDigest { logs: vec![] });
		let api = TestApi {
			genesis,
			config: params,
			snapshot: None,
			parent_header: Some(parent_header),
		};
		let res = run_verify(api, h, genesis);
		assert!(res.is_err(), "expected error, got ok");
		let err = res.err().unwrap();
		assert!(err.contains("schedule snapshot not available"), "got: {}", err);
	}

	#[test]
	fn verifier_parent_lookup_error() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let snapshot = build_snapshot(&keys, &stakes, &params, slot);
		let api = TestApi {
			genesis,
			config: params,
			snapshot: Some(snapshot),
			parent_header: None,
		};
		let res = run_verify(api, h, genesis);
		assert!(res.is_err(), "expected error, got ok");
		let err = res.err().unwrap();
		assert!(err.contains("parent header not found"), "got: {}", err);
	}

	#[test]
	fn verifier_fails_on_bad_signature_through_adapter() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		// Sign with a non-leader key (index 3) so the seal is invalid.
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 3, |_| {});
		let snapshot = build_snapshot(&keys, &stakes, &params, slot);
		let parent_header =
			OpaqueHeader::new(0u32, h256(0), h256(0), h256(0), GenericDigest { logs: vec![] });
		let api = TestApi {
			genesis,
			config: params,
			snapshot: Some(snapshot),
			parent_header: Some(parent_header),
		};
		let res = run_verify(api, h, genesis);
		assert!(res.is_err(), "expected error, got ok");
		let err = res.err().unwrap();
		assert!(err.contains("seal verification failed"), "got: {}", err);
	}

	/// Prove the verifier reads the *frozen* snapshot params and not the live,
	/// mutable `configuration()`: build a valid header with params `P1`, put `P1`
	/// into the snapshot, but have `configuration()` return a different `P2`. If
	/// the verifier incorrectly used `configuration()`, the schedule root would be
	/// recomputed under `P2` and mismatch the signed root -> rejection. Reading the
	/// snapshot params yields a match -> acceptance.
	#[test]
	fn verifier_uses_snapshot_params_not_configuration() {
		let (keys, stakes, params, genesis) = fixtures();
		let parent = h256(0xbb);
		let slot = Slot::from(7u64);
		let (h, _) = build_header(&keys, &stakes, &params, genesis, parent, slot, 0, |_| {});
		let snapshot = build_snapshot(&keys, &stakes, &params, slot);

		// A divergent `configuration()` (window_len 1 instead of 2) — must be
		// ignored by the verifier.
		let divergent_config =
			SunLeadScheduleParams { epoch_slots: 600, window_len: 1, max_window_slots: 4 };
		let parent_header =
			OpaqueHeader::new(0u32, h256(0), h256(0), h256(0), GenericDigest { logs: vec![] });
		let api = TestApi {
			genesis,
			config: divergent_config,
			snapshot: Some(snapshot),
			parent_header: Some(parent_header),
		};
		assert!(run_verify(api, h, genesis).is_ok());
	}
}
