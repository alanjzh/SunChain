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
use sp_core::H256;
use sp_runtime::traits::Header as HeaderT;
use std::sync::Arc;
use sunchain_runtime::apis::SunLeadApi;
use sunchain_runtime::opaque::Block;
use sunlead_primitives::{
	SunLeadPreDigest, SunLeadScheduleContext, SunLeadScheduleParams, SUNLEAD_ENGINE_ID,
};
use sunlead_verifier::{verify_parent_hash, verify_pre_digest, verify_seal};

use crate::sunlead_consensus::block_import::SunLeadBlockImport;

/// Concrete header type for this chain's opaque block.
type Header = <Block as sp_runtime::traits::Block>::Header;

/// The hash a leader signs over: the block header *with* the SunLead seal
/// removed from its digest (the pre-seal hash).
///
/// The seal lives in a `DigestItem::Seal`, so stripping it and re-hashing yields
/// exactly the header hash the leader bound its signature to.
fn pre_seal_hash(header: &Header) -> H256 {
	let mut header = header.clone();
	header.digest_mut().logs.retain(
		|log| log.seal_try_to::<sunlead_primitives::SunLeadSignature>(&SUNLEAD_ENGINE_ID).is_none(),
	);
	H256::from_slice(header.hash().as_ref())
}

/// Verifier that checks the SunLead pre-digest attached to every SunLead-authored
/// block.
pub struct SunLeadVerifier<C> {
	client: Arc<C>,
	genesis_hash: H256,
}

impl<C> SunLeadVerifier<C> {
	/// Build a verifier bound to `client` and the chain `genesis_hash` (bound into
	/// the digest signature for cross-chain replay protection).
	pub fn new(client: Arc<C>, genesis_hash: H256) -> Self {
		Self { client, genesis_hash }
	}
}

/// Locate the single SunLead pre-runtime digest in a header, rejecting multiple
/// or missing ones.
fn find_sunlead_pre_digest(header: &Header) -> Result<SunLeadPreDigest, ConsensusError> {
	let mut found: Option<SunLeadPreDigest> = None;
	for log in header.digest().logs() {
		if let Some(d) = log.pre_runtime_try_to::<SunLeadPreDigest>(&SUNLEAD_ENGINE_ID) {
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

/// Extract the SunLead seal (a `DigestItem::Seal(SUNLEAD_ENGINE_ID, ..)`) from a
/// header.
fn find_sunlead_seal(header: &Header) -> Option<sunlead_primitives::SunLeadSignature> {
	header
		.digest()
		.logs()
		.iter()
		.find_map(|log| log.seal_try_to::<sunlead_primitives::SunLeadSignature>(&SUNLEAD_ENGINE_ID))
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
		let pre_digest =
			find_sunlead_pre_digest(&header).map_err(|e| e.to_string())?;
		let parent_hash = *header.parent_hash();
		let slot = pre_digest.body().slot;

		let api = self.client.runtime_api();
		let snapshot = api
			.current_snapshot(parent_hash)
			.map_err(|e| format!("sunlead: failed to fetch snapshot: {:?}", e))?
			.ok_or_else(|| "sunlead: schedule snapshot not available".to_string())?;
		let params: SunLeadScheduleParams =
			api.configuration(parent_hash).map_err(|e| format!("sunlead: failed to fetch config: {:?}", e))?;

		// Reconstruct the verification context from the on-chain snapshot.
		let context = SunLeadScheduleContext {
			chain_id: snapshot.chain_id,
			authority_set_id: snapshot.authority_set_id,
			epoch_index: snapshot.epoch_index,
			epoch_seed: snapshot.epoch_seed,
			authorities: snapshot
				.authorities
				.iter()
				.map(|a| (a.id.clone(), a.stake))
				.collect(),
		};

		let pre_seal_hash = pre_seal_hash(&header);

		verify_pre_digest(&self.genesis_hash, &pre_seal_hash, &pre_digest, &context, &params)
			.map_err(|e| format!("sunlead: pre-digest verification failed: {:?}", e))?;

		// The leader's seal signs the pre-seal header hash; it lives in the
		// post-digest seal, not inside the pre-digest body (to avoid a circular
		// dependency on the hash it signs).
		let seal = find_sunlead_seal(&header)
			.ok_or_else(|| "sunlead: missing seal in header".to_string())?;
		verify_seal(self.genesis_hash, pre_seal_hash, &pre_digest, &context, &seal)
			.map_err(|e| format!("sunlead: seal verification failed: {:?}", e))?;

		verify_parent_hash(pre_digest.body().parent_hash, H256::from_slice(parent_hash.as_ref()))
			.map_err(|e| format!("sunlead: parent hash mismatch: {:?}", e))?;

		// Slot monotonicity: the parent's slot must be strictly less than this slot.
		if let Ok(Some(parent_header)) = self.client.header(parent_hash) {
			if let Ok(parent_pre) = find_sunlead_pre_digest(&parent_header) {
				sunlead_verifier::verify_slot_monotonicity(parent_pre.body().slot, slot)
					.map_err(|e| format!("sunlead: slot monotonicity violated: {:?}", e))?;
			}
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
	let verifier = SunLeadVerifier::new(client, genesis_hash);
	BasicQueue::new(verifier, Box::new(block_import), justification_import, spawner, registry)
}
