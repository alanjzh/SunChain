//! SunLead authoring worker + `start_sunlead`.
//!
//! Implements [`sc_consensus_slots::SimpleSlotWorker`] for SunLead: at every slot
//! the node derives the deterministic schedule from the parent-state snapshot and,
//! if it is the scheduled leader, builds a block carrying a SunLead pre-digest and
//! a seal (signature) over the pre-seal header hash. Authoring is gated by the
//! runtime `SunLeadApi::is_active()` flag (wired in `service.rs`); this module
//! only provides the machinery.
//!
//! This is a *parallel* module: it does not touch BABE/GRANDPA and is not the
//! default authoring engine.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::debug;
use sc_consensus::BlockImport as BlockImportT;
use sc_consensus::StateAction;
use sc_consensus_slots::{proposing_remaining_duration, SlotLenienceType, SlotProportion};
use sc_telemetry::TelemetryHandle;
use futures::future::TryFutureExt;
use scale_codec::Encode;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::{HeaderBackend, HeaderMetadata};
use sp_consensus::{BlockOrigin, Environment, Error as ConsensusError, Proposer, SelectChain, SyncOracle};
use sp_consensus_slots::Slot;
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::{KeystorePtr, Keystore};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, NumberFor};
use sp_core::H256;
use sp_runtime::DigestItem;
use std::future::Future;

use sunchain_runtime::apis::SunLeadApi;
use sunchain_runtime::opaque::Block;
use sunlead_primitives::{
	signing_message, SunLeadId, SunLeadPreDigest, SunLeadPreDigestBody, SunLeadScheduleContext,
	SunLeadScheduleParams, SunLeadSignature, KEY_TYPE, SUNLEAD_ENGINE_ID, SUNLEAD_PROTOCOL_VERSION,
};
use sunlead_schedule::{build_schedule, ComputedSchedule, ScheduleInput};

use crate::sunlead_consensus::block_import::SunLeadBlockImport;

/// The data the worker needs to author (and later sign) a SunLead block.
#[derive(Clone)]
pub struct SunLeadClaim {
	/// The unsigned pre-digest body that will be placed in the header.
	pub body: SunLeadPreDigestBody,
	/// The leader's `sunl` authority id (used to sign + identify the key).
	pub leader_id: SunLeadId,
}

/// Auxiliary data derived from the parent state that the worker needs to claim a
/// slot and author a block.
pub struct SunLeadAuxData {
	/// Verification context (authorities, set id, epoch, seed) for this schedule.
	pub context: SunLeadScheduleContext,
	/// Schedule parameters for the epoch.
	pub params: SunLeadScheduleParams,
	/// The fully derived deterministic schedule.
	pub schedule: ComputedSchedule,
}

/// Extract the slot of a SunLead pre-digest from a header, if present.
fn find_sunlead_slot<H: HeaderT>(header: &H) -> Option<Slot> {
	header.digest().logs().iter().find_map(|log| {
		log.pre_runtime_try_to::<SunLeadPreDigest>(&SUNLEAD_ENGINE_ID).map(|d| d.body().slot)
	})
}

/// The SunLead slot worker. Implements [`sc_consensus_slots::SimpleSlotWorker`].
pub struct SunLeadSlotWorker<B: BlockT, C, E, I, SO, L, BS> {
	client: Arc<C>,
	block_import: SunLeadBlockImport<B, I>,
	env: E,
	sync_oracle: SO,
	justification_sync_link: L,
	force_authoring: bool,
	backoff_authoring_blocks: Option<BS>,
	keystore: KeystorePtr,
	genesis_hash: H256,
	block_proposal_slot_portion: SlotProportion,
	max_block_proposal_slot_portion: Option<SlotProportion>,
	telemetry: Option<TelemetryHandle>,
	_phantom: PhantomData<(B, E, I, SO, L, BS)>,
}

#[async_trait::async_trait]
impl<B, C, E, I, SO, L, BS, Error> sc_consensus_slots::SimpleSlotWorker<B>
	for SunLeadSlotWorker<B, C, E, I, SO, L, BS>
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + HeaderMetadata<B, Error = sp_blockchain::Error> + Send + Sync + 'static,
	C::Api: SunLeadApi<B>,
	E: sp_consensus::Environment<B, Error = Error> + Send + Sync + 'static,
	E::Proposer: sp_consensus::Proposer<B, Error = Error> + Send,
	I: BlockImportT<B, Error = ConsensusError> + Send + Sync + 'static,
	SO: SyncOracle + Send + Clone + Sync,
	L: sc_consensus::JustificationSyncLink<B>,
	BS: sc_consensus_slots::BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + Sync,
	Error: std::error::Error + Send + From<ConsensusError> + From<I::Error> + 'static,
{
	type Claim = SunLeadClaim;
	type SyncOracle = SO;
	type JustificationSyncLink = L;
	type CreateProposer =
		std::pin::Pin<Box<dyn Future<Output = Result<E::Proposer, ConsensusError>> + Send + 'static>>;
	type Proposer = E::Proposer;
	type BlockImport = SunLeadBlockImport<B, I>;
	type AuxData = SunLeadAuxData;

	fn logging_target(&self) -> &'static str {
		"sunlead"
	}

	fn block_import(&mut self) -> &mut Self::BlockImport {
		&mut self.block_import
	}

	fn aux_data(&self, parent: &B::Header, _slot: Slot) -> Result<Self::AuxData, ConsensusError> {
		let api = self.client.runtime_api();
		let parent_hash = parent.hash();
		let snapshot = api
			.current_snapshot(parent_hash.clone())
			.map_err(|e| ConsensusError::ChainLookup(format!("sunlead: {:?}", e)))?
			.ok_or(ConsensusError::InvalidAuthoritiesSet)?;
		let params: SunLeadScheduleParams = api
			.configuration(parent_hash)
			.map_err(|e| ConsensusError::ChainLookup(format!("sunlead: {:?}", e)))?;

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

		let input = ScheduleInput {
			chain_id: context.chain_id,
			authority_set_id: context.authority_set_id,
			epoch_index: context.epoch_index,
			epoch_seed: context.epoch_seed,
			authorities: context.authorities.clone(),
			params,
		};
		let schedule = build_schedule(&input).map_err(|e| {
			ConsensusError::ClientImport(format!("sunlead: schedule build failed: {:?}", e))
		})?;

		debug!(target: "sunlead", "built SunLead schedule for epoch {}", context.epoch_index);
		Ok(SunLeadAuxData { context, params, schedule })
	}

	fn authorities_len(&self, aux: &Self::AuxData) -> Option<usize> {
		Some(aux.context.authorities.len())
	}

	async fn claim_slot(
		&mut self,
		header: &B::Header,
		slot: Slot,
		aux: &Self::AuxData,
	) -> Option<Self::Claim> {
		// The runtime slot clock is absolute (anchored to the Unix epoch), but the
		// SunLead schedule and its verification are defined per-epoch as
		// `slot % epoch_slots`. Map the absolute slot into the current epoch.
		let rel_slot = Slot::from(*slot % aux.params.epoch_slots);
		debug!(target: "sunlead", "attempting to claim slot {} (epoch-relative {})", slot, rel_slot);
		let (leader_index, window_id) = aux.schedule.leader_at(rel_slot.into())?;
		let leader_id = aux.context.authorities.get(leader_index as usize)?.0.clone();

		// Only claim if this node controls the scheduled leader's `sunl` key.
		let leader_pub = sp_core::sr25519::Public::try_from(leader_id.as_ref()).ok();
		let my_keys = self.keystore.sr25519_public_keys(KEY_TYPE);
		if !my_keys.iter().any(|k| Some(k) == leader_pub.as_ref()) {
			return None
		}

		let body = SunLeadPreDigestBody {
			version: SUNLEAD_PROTOCOL_VERSION,
			authority_set_id: aux.context.authority_set_id,
			epoch_index: aux.context.epoch_index,
			slot,
			window_id,
			leader_index,
			parent_hash: H256::from_slice(header.hash().as_ref()),
			schedule_root: aux.schedule.root,
		};
		debug!(target: "sunlead", "claimed slot {} as leader index {}", slot, leader_index);
		Some(SunLeadClaim { body, leader_id })
	}

	fn pre_digest_data(&self, _slot: Slot, claim: &Self::Claim) -> Vec<DigestItem> {
		vec![DigestItem::PreRuntime(
			SUNLEAD_ENGINE_ID,
			SunLeadPreDigest(claim.body.clone()).encode(),
		)]
	}

	async fn block_import_params(
		&self,
		header: B::Header,
		header_hash: &B::Hash,
		body: Vec<B::Extrinsic>,
		storage_changes: sc_consensus_slots::StorageChanges<B>,
		claim: Self::Claim,
		_aux: Self::AuxData,
	) -> Result<sc_consensus::BlockImportParams<B>, ConsensusError> {
		// `header_hash` is the pre-seal hash (the header without the seal), which
		// is exactly what the leader binds its signature to.
		let pre_seal_hash = H256::from_slice(header_hash.as_ref());
		let msg = signing_message(&self.genesis_hash, &pre_seal_hash, &claim.body);
		let sig = self
			.keystore
			.sr25519_sign(KEY_TYPE, claim.leader_id.as_ref(), msg.as_ref())
			.map_err(|e| ConsensusError::CannotSign(format!("sunlead: {:?}", e)))?
			.ok_or_else(|| ConsensusError::CannotSign("sunlead: key not found in keystore".into()))?;
		let seal = SunLeadSignature::from(sig);
		let seal_item = DigestItem::Seal(SUNLEAD_ENGINE_ID, seal.encode());

		let mut import_block = sc_consensus::BlockImportParams::new(BlockOrigin::Own, header);
		import_block.post_digests.push(seal_item);
		import_block.body = Some(body);
		import_block.state_action = StateAction::ApplyChanges(
			sc_consensus::StorageChanges::Changes(storage_changes),
		);
		Ok(import_block)
	}

	fn force_authoring(&self) -> bool {
		self.force_authoring
	}

	fn should_backoff(&self, slot: Slot, chain_head: &B::Header) -> bool {
		if let Some(ref strategy) = self.backoff_authoring_blocks {
			if let Some(chain_head_slot) = find_sunlead_slot(chain_head) {
				return strategy.should_backoff(
					*chain_head.number(),
					chain_head_slot,
					self.client.info().finalized_number,
					slot,
					self.logging_target(),
				)
			}
		}
		false
	}

	fn sync_oracle(&mut self) -> &mut Self::SyncOracle {
		&mut self.sync_oracle
	}

	fn justification_sync_link(&mut self) -> &mut Self::JustificationSyncLink {
		&mut self.justification_sync_link
	}

	fn proposer(&mut self, block: &B::Header) -> Self::CreateProposer {
		Box::pin(
			self.env
				.init(block)
				.map_err(|e| ConsensusError::ClientImport(e.to_string())),
		)
	}

	fn telemetry(&self) -> Option<TelemetryHandle> {
		self.telemetry.clone()
	}

	fn proposing_remaining_duration(
		&self,
		slot_info: &sc_consensus_slots::SlotInfo<B>,
	) -> Duration {
		let parent_slot = find_sunlead_slot(&slot_info.chain_head);
		proposing_remaining_duration(
			parent_slot,
			slot_info,
			&self.block_proposal_slot_portion,
			self.max_block_proposal_slot_portion.as_ref(),
			SlotLenienceType::Exponential,
			self.logging_target(),
		)
	}
}

/// Parameters for [`start_sunlead`].
pub struct SunLeadParams<B: BlockT, C, SC, E, I, SO, CIDP, BS, L> {
	/// The keystore holding the node's `sunl` session keys.
	pub keystore: KeystorePtr,
	/// Client for querying the runtime (snapshot / config / activation).
	pub client: Arc<C>,
	/// Select-chain used by the slot loop.
	pub select_chain: SC,
	/// Environment (proposer factory) used to build blocks.
	pub env: E,
	/// The SunLead block import wrapping the chain's effective import.
	pub block_import: SunLeadBlockImport<B, I>,
	/// Sync oracle used to skip authoring while syncing.
	pub sync_oracle: SO,
	/// Link used to drive justification sync on import.
	pub justification_sync_link: L,
	/// Inherent-data provider factory (slot + timestamp).
	pub create_inherent_data_providers: CIDP,
	/// Whether to force authoring even while offline.
	pub force_authoring: bool,
	/// Optional backoff strategy.
	pub backoff_authoring_blocks: Option<BS>,
	/// Chain genesis hash (bound into the seal signature).
	pub genesis_hash: H256,
	/// Slot duration for the SunLead slot loop.
	pub slot_duration: sp_consensus_slots::SlotDuration,
	/// Proportion of a slot dedicated to proposing.
	pub block_proposal_slot_portion: SlotProportion,
	/// Max proportion of a slot for proposing (with lenience).
	pub max_block_proposal_slot_portion: Option<SlotProportion>,
	/// Telemetry handle.
	pub telemetry: Option<TelemetryHandle>,
}

/// Start the SunLead authoring worker. Returns a future that must be spawned on an
/// essential task (it loops until the node stops).
pub fn start_sunlead<B, C, SC, E, I, SO, CIDP, BS, L, Error>(
	params: SunLeadParams<B, C, SC, E, I, SO, CIDP, BS, L>,
) -> impl Future<Output = ()> + Send + 'static
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + HeaderMetadata<B, Error = sp_blockchain::Error> + Send + Sync + 'static,
	C::Api: SunLeadApi<B>,
	SC: SelectChain<B> + 'static,
	E: sp_consensus::Environment<B, Error = Error> + Send + Sync + 'static,
	E::Proposer: sp_consensus::Proposer<B, Error = Error> + Send,
	I: BlockImportT<B, Error = ConsensusError> + Send + Sync + 'static,
	SO: SyncOracle + Send + Sync + Clone + 'static,
	L: sc_consensus::JustificationSyncLink<B> + 'static,
	CIDP: CreateInherentDataProviders<B, ()> + Send + Sync + 'static,
	CIDP::InherentDataProviders: sc_consensus_slots::InherentDataProviderExt + Send,
	BS: sc_consensus_slots::BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + Sync + 'static,
	Error: std::error::Error + Send + From<ConsensusError> + From<I::Error> + 'static,
{
	let worker = SunLeadSlotWorker::<B, C, E, I, SO, L, BS> {
		client: params.client,
		block_import: params.block_import,
		env: params.env,
		sync_oracle: params.sync_oracle.clone(),
		justification_sync_link: params.justification_sync_link,
		force_authoring: params.force_authoring,
		backoff_authoring_blocks: params.backoff_authoring_blocks,
		keystore: params.keystore,
		genesis_hash: params.genesis_hash,
		block_proposal_slot_portion: params.block_proposal_slot_portion,
		max_block_proposal_slot_portion: params.max_block_proposal_slot_portion,
		telemetry: params.telemetry,
		_phantom: PhantomData,
	};

	debug!(target: "sunlead", "👶 Starting SunLead authorship worker");

	sc_consensus_slots::start_slot_worker(
		params.slot_duration,
		params.select_chain,
		sc_consensus_slots::SimpleSlotWorkerToSlotWorker(worker),
		params.sync_oracle,
		params.create_inherent_data_providers,
	)
}
