//! RPC entry point for submitting signed exchange actions (P2-D §12.1 step ①).

use std::sync::{Arc, Mutex};

use jsonrpsee::{
	core::{async_trait, RpcResult},
	proc_macros::rpc,
	types::{ErrorObject, ErrorObjectOwned},
};
use scale_codec::Decode;
use serde::{Deserialize, Serialize};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::Bytes;

use sunchain_runtime::apis::{
	ActionOutcome, ActionReceipt, ExchangeRuntimeApi, ExchangeSignedAction,
};

use crate::exchange_pool::ExchangePool;

/// serde view of `pallet_exchange_core::ActionReceipt` (S1-5 / S2-5).
///
/// The runtime type is not serde; this mirror keeps the RPC free of a direct serde dependency on
/// the runtime crate and lets the `error` code stay a clean array rather than a SCALE blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReceiptRpc {
	/// The action's nonce within its sub-account.
	pub action_nonce: u64,
	/// Block height at which the action was resolved.
	pub block: u32,
	/// `Applied` (executed), `Refused` (bad signature / expired / wrong sub-account / nonce not
	/// ahead — the nonce is still free) or `Rejected` (business part rolled back — the nonce is
	/// spent).
	pub outcome: String,
	/// Whether this action consumed its nonce. The difference between `Refused` (false) and
	/// `Rejected` (true) is the whole point: a client cannot otherwise tell "retry" from
	/// "re-sign".
	pub nonce_consumed: bool,
	/// `(pallet_index, error_code)` when the action failed; absent on success.
	pub error: Option<(u8, [u8; 4])>,
}

/// The status of one action, as the client needs to see it (S2-5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
	/// No receipt for this nonce: it was never pooled, or its sub-account/nonce is unknown here.
	NotPooled,
	/// Applied and included in a block.
	Included,
	/// Refused before the nonce was touched: bad signature, expired, wrong sub-account, or a
	/// nonce that is not ahead. The nonce is still free.
	Refused,
	/// Accepted into the book then rolled back; the nonce is spent. Re-submitting the same nonce
	/// will be rejected.
	Rejected,
}

/// Submitting an action here is the only way into the dedicated pool.
#[rpc(client, server)]
pub trait ExchangeApi {
	/// Queues a signed exchange action for inclusion in the next exchange batch.
	///
	/// The payload is the SCALE encoding of `SignedExchangeAction`. Passing it as opaque
	/// bytes keeps this RPC from having to mirror the runtime's types in serde — and from
	/// going stale every time those types change.
	///
	/// A success means "accepted into the pool", not "will execute". The runtime replay
	/// inside `execute_exchange_batch` is the only authority (§12.2).
	#[method(name = "exchange_submitAction")]
	fn submit_action(&self, encoded: Bytes) -> RpcResult<()>;

	/// Receipts for one account's sub-account, oldest first (S1-5 / S2-5).
	///
	/// `who` is the SCALE encoding of the account id (20 bytes). A batch dispatched `Ok` even
	/// when every action inside was refused, so this is the only way to learn what actually
	/// happened to an action and — crucially — whether its nonce was spent.
	#[method(name = "exchange_actionReceipts")]
	fn action_receipts(&self, who: Bytes, subaccount: u32) -> RpcResult<Vec<ActionReceiptRpc>>;

	/// The status of a single action (S2-5): what the client must know to decide "retry" vs
	/// "re-sign". Derived from [`ExchangeApi::action_receipts`].
	#[method(name = "exchange_actionStatus")]
	fn action_status(&self, who: Bytes, subaccount: u32, nonce: u64) -> RpcResult<ActionStatus>;
}

/// Maps a runtime `ActionReceipt` to its serde mirror.
fn to_rpc_receipt(r: ActionReceipt) -> ActionReceiptRpc {
	let (outcome, nonce_consumed) = match r.outcome {
		ActionOutcome::Applied => ("applied".into(), true),
		ActionOutcome::Refused => ("refused".into(), false),
		ActionOutcome::Rejected => ("rejected".into(), true),
	};
	ActionReceiptRpc {
		action_nonce: r.action_nonce,
		block: r.block,
		outcome,
		nonce_consumed,
		error: r.error,
	}
}

pub struct ExchangeRpc<C, B> {
	pool: Arc<Mutex<ExchangePool>>,
	client: Arc<C>,
	/// Actions accepted locally are handed to the gossip task for propagation.
	outbound: futures::channel::mpsc::UnboundedSender<Vec<u8>>,
	_marker: std::marker::PhantomData<B>,
}

impl<C, B> ExchangeRpc<C, B> {
	pub fn new(
		pool: Arc<Mutex<ExchangePool>>,
		client: Arc<C>,
		outbound: futures::channel::mpsc::UnboundedSender<Vec<u8>>,
	) -> Self {
		Self { pool, client, outbound, _marker: Default::default() }
	}
}

#[async_trait]
impl<C, B> ExchangeApiServer for ExchangeRpc<C, B>
where
	B: sp_runtime::traits::Block,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
	C::Api: ExchangeRuntimeApi<B>,
{
	fn submit_action(&self, encoded: Bytes) -> RpcResult<()> {
		let action: ExchangeSignedAction = Decode::decode(&mut &encoded[..])
			.map_err(|e| rpc_error(format!("payload is not a SignedExchangeAction: {e}")))?;

		// Pre-validate (§12.2) — advisory: fast feedback, and keeps junk out of the pool.
		let at = self.client.info().best_hash;
		let valid = self
			.client
			.runtime_api()
			.validate_exchange_action(at, action.clone())
			.map_err(|e| rpc_error(format!("runtime api unavailable: {e}")))?
			.is_ok();

		let mut pool = self.pool.lock().map_err(|_| rpc_error("pool lock poisoned".into()))?;
		let encoded = scale_codec::Encode::encode(&action);
		pool.submit(action, |_| valid)
			.map_err(|reject| rpc_error(format!("rejected by the pool: {reject:?}")))?;

		// Let the other nodes know. Failing to reach them is not a submission failure.
		let _ = self.outbound.unbounded_send(encoded);
		Ok(())
	}

	fn action_receipts(&self, who: Bytes, subaccount: u32) -> RpcResult<Vec<ActionReceiptRpc>> {
		let who = sunchain_runtime::AccountId::decode(&mut &who[..])
			.map_err(|e| rpc_error(format!("who is not an account id: {e}")))?;
		let at = self.client.info().best_hash;
		let receipts = self
			.client
			.runtime_api()
			.action_receipts(at, who, subaccount)
			.map_err(|e| rpc_error(format!("runtime api unavailable: {e}")))?;
		Ok(receipts.into_iter().map(to_rpc_receipt).collect())
	}

	fn action_status(
		&self,
		who: Bytes,
		subaccount: u32,
		nonce: u64,
	) -> RpcResult<ActionStatus> {
		let who = sunchain_runtime::AccountId::decode(&mut &who[..])
			.map_err(|e| rpc_error(format!("who is not an account id: {e}")))?;
		let at = self.client.info().best_hash;
		let receipts = self
			.client
			.runtime_api()
			.action_receipts(at, who, subaccount)
			.map_err(|e| rpc_error(format!("runtime api unavailable: {e}")))?;
		let status = match receipts.into_iter().find(|r| r.action_nonce == nonce) {
			None => ActionStatus::NotPooled,
			Some(r) => match r.outcome {
				ActionOutcome::Applied => ActionStatus::Included,
				ActionOutcome::Refused => ActionStatus::Refused,
				ActionOutcome::Rejected => ActionStatus::Rejected,
			},
		};
		Ok(status)
	}
}

fn rpc_error(message: String) -> ErrorObjectOwned {
	ErrorObject::owned(1, message, None::<()>)
}
