//! Deterministic packaging of exchange actions into an `Exchange Batch` (P2-D §12.1 ③, §12.4).
//!
//! The proposer drains the dedicated [`ExchangePool`], wraps what it found into one
//! `execute_exchange_batch` extrinsic and hands it to the Substrate pool before the inner
//! proposer starts building. The extrinsic is *unsigned*: a block author's consensus key
//! cannot sign an extrinsic, and §12.4 deliberately gives authorisation to each action's
//! own signature instead.
//!
//! This is *not* responsible for execution order — the runtime re-sorts the batch by
//! `(class, nonce, arrival)` regardless. It only decides which actions get a seat when there
//! is more demand than one batch can hold, and it does that from the same sorted view, so
//! the choice cannot favour whoever happened to submit first.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use scale_codec::{Decode, Encode};
use sc_transaction_pool_api::{TransactionPool, TransactionSource};
use sp_consensus::{Environment, Proposal, Proposer};
use sp_runtime::{
	generic::Digest,
	traits::{Block as BlockT, Header as _},
};

use crate::exchange_pool::ExchangePool;

/// Wraps the node's real proposer factory, injecting one exchange batch per slot.
pub struct ExchangeProposerFactory<E, P> {
	inner: E,
	exchange: Arc<std::sync::Mutex<ExchangePool>>,
	pool: Arc<P>,
}

impl<E, P> ExchangeProposerFactory<E, P> {
	pub fn new(inner: E, exchange: Arc<std::sync::Mutex<ExchangePool>>, pool: Arc<P>) -> Self {
		Self { inner, exchange, pool }
	}
}

impl<B, E, P> Environment<B> for ExchangeProposerFactory<E, P>
where
	B: BlockT<Extrinsic = sp_runtime::OpaqueExtrinsic>,
	E: Environment<B> + Send + 'static,
	P: TransactionPool<Block = B> + Send + Sync + 'static,
{
	type Proposer = ExchangeProposer<B, E::Proposer, P>;
	type CreateProposer =
		Pin<Box<dyn Future<Output = Result<Self::Proposer, Self::Error>> + Send + 'static>>;
	type Error = E::Error;

	fn init(&mut self, parent_header: &B::Header) -> Self::CreateProposer {
		let inner = self.inner.init(parent_header);
		let exchange = self.exchange.clone();
		let pool = self.pool.clone();
		let parent_hash = parent_header.hash();
		Box::pin(async move {
			Ok(ExchangeProposer { inner: inner.await?, parent_hash, exchange, pool })
		})
	}
}

/// The proposer that actually injects the batch.
pub struct ExchangeProposer<B: BlockT, P, TP> {
	inner: P,
	parent_hash: B::Hash,
	exchange: Arc<std::sync::Mutex<ExchangePool>>,
	pool: Arc<TP>,
}

impl<B, P, TP> Proposer<B> for ExchangeProposer<B, P, TP>
where
	B: BlockT<Extrinsic = sp_runtime::OpaqueExtrinsic>,
	P: Proposer<B> + Send + 'static,
	TP: TransactionPool<Block = B> + Send + Sync + 'static,
{
	type Error = P::Error;
	type Proposal =
		Pin<Box<dyn Future<Output = Result<Proposal<B, Self::Proof>, Self::Error>> + Send + 'static>>;
	type ProofRecording = P::ProofRecording;
	type Proof = P::Proof;

	/// Note `self` by value — that is the signature in this SDK version.
	fn propose(
		self,
		inherent_data: sp_inherents::InherentData,
		inherent_digests: Digest,
		max_duration: Duration,
		block_size_limit: Option<usize>,
	) -> Self::Proposal {
		let Self { inner, parent_hash, exchange, pool } = self;

		// Read, do not remove: an action stays queued until a background task sees the
		// chain consume it. Taking it here would lose it whenever this block is discarded.
		let actions: Vec<_> = exchange
			.lock()
			.map(|p| p.pending())
			.unwrap_or_default()
			.into_iter()
			.map(|(_key, action)| action)
			.collect();

		// Created now, polled later — so the submission below lands before the inner
		// proposer collects from the pool.
		let inner_fut =
			inner.propose(inherent_data, inherent_digests, max_duration, block_size_limit);

		Box::pin(async move {
			if !actions.is_empty() {
				log::info!(target: "exchange", "proposing with {} queued action(s)", actions.len());
				match unsigned_batch(actions) {
					Some(xt) => {
						let before = pool.status().ready;
						let res = pool.submit_one(parent_hash, TransactionSource::Local, xt).await;
						log::info!(target: "exchange",
							"pool ready before={} after={}", before, pool.status().ready);
						match res
						{
						Ok(_) => log::info!(target: "exchange", "batch accepted by the pool"),
						Err(e) => {
							// Offering the same batch again is expected: the previous one may
							// already be in the pool or in a block. Only a new failure is news.
							let text = format!("{e:?}");
							if text.contains("TemporarilyBanned") || text.contains("AlreadyImported") {
								log::debug!(target: "exchange", "batch already present: {text}");
							} else {
								log::warn!(target: "exchange", "batch rejected by pool: {text}");
							}
						}
						}
					},
					None => log::warn!(target: "exchange", "could not build the batch extrinsic"),
				}
			}
			inner_fut.await
		})
	}
}

/// Builds the unsigned `execute_exchange_batch` extrinsic for this block type.
///
/// The node's `Block` is the opaque one, so the typed extrinsic is encoded and re-decoded
/// into the block's own extrinsic representation.
fn unsigned_batch(
	actions: Vec<sunchain_runtime::apis::ExchangeSignedAction>,
) -> Option<sp_runtime::OpaqueExtrinsic> {
	if actions.is_empty() {
		return None
	}
	let bounded = frame_support::BoundedVec::truncate_from(actions);
	let call = sunchain_runtime::RuntimeCall::ExchangeCore(
		pallet_exchange_core::Call::execute_exchange_batch { actions: bounded },
	);
	let typed = sunchain_runtime::UncheckedExtrinsic::new_bare(call);
	let encoded = typed.encode();
	log::info!(target: "exchange", "batch extrinsic is {} bytes", encoded.len());
	// `OpaqueExtrinsic::from_bytes` is the supported way in: it wraps the raw encoding.
	// Decoding the bytes *as* an OpaqueExtrinsic is not equivalent, and silently produced
	// something the block could never dispatch.
	match sp_runtime::OpaqueExtrinsic::from_bytes(&encoded) {
		Ok(xt) => Some(xt),
		Err(_) => {
			log::error!(target: "exchange", "could not wrap the batch encoding");
			None
		},
	}
}
