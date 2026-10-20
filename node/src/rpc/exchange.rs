//! RPC entry point for submitting signed exchange actions (P2-D §12.1 step ①).

use std::sync::{Arc, Mutex};

use jsonrpsee::{
	core::{async_trait, RpcResult},
	proc_macros::rpc,
	types::{ErrorObject, ErrorObjectOwned},
};
use scale_codec::Decode;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::Bytes;

use sunchain_runtime::apis::{ExchangeRuntimeApi, ExchangeSignedAction};

use crate::exchange_pool::ExchangePool;

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
}

fn rpc_error(message: String) -> ErrorObjectOwned {
	ErrorObject::owned(1, message, None::<()>)
}
