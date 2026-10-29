//! SunLead diagnostics RPCs (P1-7.3).
//!
//! Four read-only methods that let an operator (and the `admin` CLI) observe the
//! deterministic schedule without re-deriving it:
//!
//! * `sunlead_status` — is SunLead active, is a snapshot present, current epoch /
//!   authority-set ids, schedule root, and the best block's absolute slot.
//! * `sunlead_epoch` — the frozen epoch parameters, absolute start/end slots,
//!   schedule root, authority set, and the full (id, stake) authority list.
//! * `sunlead_nextLeader` — the scheduled `(slot, leader_index, window_id)` for a
//!   run of *absolute* slots (defaults to the slots after the best block).
//! * `sunlead_schedule` — the Merkle proof for one *absolute* slot, plus the
//!   leader/window it commits to (errors out if the slot is out of range / no
//!   snapshot).
//!
//! All `slot` arguments are **absolute** slots (the same quantity the verifier
//! and leader derivation use). The schedule is frozen per epoch, so a slot's
//! leader is `leader_at(slot)` = the schedule's `slot % epoch_slots` entry.

use jsonrpsee::{
	core::{async_trait, RpcResult},
	proc_macros::rpc,
	types::{ErrorObject, ErrorObjectOwned},
};
use serde::{Deserialize, Serialize};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::H256;
use std::sync::Arc;

use sunchain_runtime::apis::SunLeadApi;
use sunchain_runtime::opaque::Block;

use crate::sunlead_consensus::import_queue::find_sunlead_slot;

/// Activation / liveness status of SunLead on this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SunLeadStatus {
	/// Whether SunLead is the currently active authoring consensus.
	pub is_active: bool,
	/// Whether an epoch authority snapshot has been built (false before the first
	/// epoch boundary / genesis build).
	pub has_snapshot: bool,
	/// Current snapshot epoch index (session counter).
	pub epoch_index: Option<u64>,
	/// Authority set id the schedule was derived from.
	pub authority_set_id: Option<u64>,
	/// Merkle root committing to the current epoch schedule.
	pub schedule_root: Option<H256>,
	/// Absolute slot of the best block (None until a SunLead block is produced).
	pub best_slot: Option<u64>,
}

/// One authority as seen by the schedule: its `sunl` key id and stake weight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SunLeadAuthorityView {
	/// `sunl` session-key id (hex).
	pub id: H256,
	/// Stake weight used for apportionment.
	pub stake: String,
}

/// The frozen epoch the current snapshot describes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SunLeadEpoch {
	pub is_active: bool,
	pub epoch_index: Option<u64>,
	/// Slots per epoch.
	pub epoch_slots: u64,
	/// Target consecutive slots per leader.
	pub window_len: u32,
	/// Hard cap on consecutive slots per leader.
	pub max_window_slots: u32,
	/// Absolute slot at which the current epoch window begins.
	pub start_slot: Option<u64>,
	/// Absolute slot at which the current epoch window ends (inclusive).
	pub end_slot: Option<u64>,
	pub schedule_root: Option<H256>,
	pub authority_set_id: Option<u64>,
	pub authorities: Vec<SunLeadAuthorityView>,
}

/// `(slot, leader_index, window_id)` for a single absolute slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SunLeadLeader {
	pub slot: u64,
	pub leader_index: u32,
	pub window_id: u32,
}

/// Merkle inclusion proof for one schedule entry, plus the leader/window it
/// commits to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SunLeadScheduleProofView {
	pub root: H256,
	pub leaf_index: u32,
	pub number_of_leaves: u32,
	pub proof: Vec<H256>,
	pub leader: u32,
	pub window_id: u32,
}

#[rpc(client, server)]
pub trait SunLeadRpcApi {
	/// SunLead activation / liveness status.
	#[method(name = "sunlead_status")]
	fn status(&self) -> RpcResult<SunLeadStatus>;

	/// The frozen epoch the current snapshot describes (params, absolute slot
	/// window, root, authority set, authorities).
	#[method(name = "sunlead_epoch")]
	fn epoch(&self) -> RpcResult<SunLeadEpoch>;

	/// The scheduled `(slot, leader_index, window_id)` for `count` absolute slots
	/// starting at `from` (default: the slot after the best block). Slots are
	/// absolute.
	#[method(name = "sunlead_nextLeader")]
	fn next_leader(&self, from: Option<u64>, count: u32) -> RpcResult<Vec<SunLeadLeader>>;

	/// Merkle proof for one absolute `slot`, plus the leader/window it commits to.
	/// Errors if the slot is out of range or no snapshot exists.
	#[method(name = "sunlead_schedule")]
	fn schedule(&self, slot: u64) -> RpcResult<SunLeadScheduleProofView>;
}

pub struct SunLeadRpc<C> {
	client: Arc<C>,
}

impl<C> SunLeadRpc<C> {
	pub fn new(client: Arc<C>) -> Self {
		Self { client }
	}
}

#[async_trait]
impl<C> SunLeadRpcApiServer for SunLeadRpc<C>
where
	C: ProvideRuntimeApi<Block> + HeaderBackend<Block> + Send + Sync + 'static,
	C::Api: SunLeadApi<Block>,
{
	fn status(&self) -> RpcResult<SunLeadStatus> {
		let at = self.client.info().best_hash;
		let api = self.client.runtime_api();
		let is_active = api.is_active(at).map_err(|e| rpc_err(format!("runtime api: {e:?}")))?;
		let snapshot = api
			.current_snapshot(at)
			.map_err(|e| rpc_err(format!("runtime api: {e:?}")))?;
		let best_slot = self
			.client
			.header(at)
			.map_err(|e| rpc_err(format!("header: {e:?}")))?
			.and_then(|h| find_sunlead_slot(&h))
			.map(|s| *s);

		Ok(match snapshot {
			Some(s) => SunLeadStatus {
				is_active,
				has_snapshot: true,
				epoch_index: Some(s.epoch_index),
				authority_set_id: Some(s.authority_set_id),
				schedule_root: Some(s.schedule_root),
				best_slot,
			},
			None => SunLeadStatus {
				is_active,
				has_snapshot: false,
				epoch_index: None,
				authority_set_id: None,
				schedule_root: None,
				best_slot,
			},
		})
	}

	fn epoch(&self) -> RpcResult<SunLeadEpoch> {
		let at = self.client.info().best_hash;
		let api = self.client.runtime_api();
		let is_active = api.is_active(at).map_err(|e| rpc_err(format!("runtime api: {e:?}")))?;
		let snapshot = api
			.current_snapshot(at)
			.map_err(|e| rpc_err(format!("runtime api: {e:?}")))?
			.ok_or_else(|| rpc_err("no schedule snapshot available"))?;

		let best_slot = self
			.client
			.header(at)
			.map_err(|e| rpc_err(format!("header: {e:?}")))?
			.and_then(|h| find_sunlead_slot(&h))
			.map(|s| *s);

		let params = snapshot.params;
		let epoch_slots = params.epoch_slots;
		let (start_slot, end_slot) = best_slot
			.map(|s| {
				let current_epoch = s / epoch_slots;
				let start = current_epoch * epoch_slots;
				(start, start + epoch_slots - 1)
			})
			.unzip();

		let authorities = snapshot
			.authorities
			.iter()
			.map(|a| SunLeadAuthorityView {
				id: H256::from_slice(a.id.as_ref()),
				stake: a.stake.to_string(),
			})
			.collect();

		Ok(SunLeadEpoch {
			is_active,
			epoch_index: Some(snapshot.epoch_index),
			epoch_slots,
			window_len: params.window_len,
			max_window_slots: params.max_window_slots,
			start_slot,
			end_slot,
			schedule_root: Some(snapshot.schedule_root),
			authority_set_id: Some(snapshot.authority_set_id),
			authorities,
		})
	}

	fn next_leader(&self, from: Option<u64>, count: u32) -> RpcResult<Vec<SunLeadLeader>> {
		let at = self.client.info().best_hash;
		let api = self.client.runtime_api();
		// Confirm a snapshot exists before promising leaders.
		let _ = api
			.current_snapshot(at)
			.map_err(|e| rpc_err(format!("runtime api: {e:?}")))?
			.ok_or_else(|| rpc_err("no schedule snapshot available"))?;

		let best_slot = self
			.client
			.header(at)
			.map_err(|e| rpc_err(format!("header: {e:?}")))?
			.and_then(|h| find_sunlead_slot(&h))
			.map(|s| *s);
		let from = from.unwrap_or_else(|| best_slot.map(|s| s + 1).unwrap_or(0));

		let mut out = Vec::with_capacity(count as usize);
		for i in 0..count as u64 {
			let slot = from + i;
			match api.leader_at(at, slot).map_err(|e| rpc_err(format!("runtime api: {e:?}")))? {
				Some((leader_index, window_id)) => {
					out.push(SunLeadLeader { slot, leader_index, window_id });
				}
				None => return Err(rpc_err(format!("no leader for slot {slot}"))),
			}
		}
		Ok(out)
	}

	fn schedule(&self, slot: u64) -> RpcResult<SunLeadScheduleProofView> {
		let at = self.client.info().best_hash;
		let api = self.client.runtime_api();
		let proof = api
			.schedule_proof(at, slot)
			.map_err(|e| rpc_err(format!("runtime api: {e:?}")))?
			.ok_or_else(|| rpc_err(format!("no schedule proof for slot {slot}")))?;
		let (leader, window_id) = api
			.leader_at(at, slot)
			.map_err(|e| rpc_err(format!("runtime api: {e:?}")))?
			.ok_or_else(|| rpc_err(format!("no leader for slot {slot}")))?;
		Ok(SunLeadScheduleProofView {
			root: proof.root,
			leaf_index: proof.leaf_index,
			number_of_leaves: proof.number_of_leaves,
			proof: proof.proof,
			leader,
			window_id,
		})
	}
}

fn rpc_err(message: impl Into<String>) -> ErrorObjectOwned {
	ErrorObject::owned(1, message.into(), None::<()>)
}
