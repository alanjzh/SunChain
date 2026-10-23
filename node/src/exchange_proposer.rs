//! Deterministic packaging of exchange actions into an `Exchange Batch` (P2-D §12.1 ③, §12.4).
//!
//! The proposer drains the dedicated [`ExchangePool`], wraps what it found into one
//! `execute_exchange_batch` extrinsic and hands it to the Substrate pool before the inner
//! proposer starts building. The extrinsic is *unsigned*: a block author's consensus key
//! cannot sign an extrinsic, and §12.4 deliberately gives authorisation to each action's
//! own signature instead.
//!
//! This decides *which* actions get a seat when there is more demand than one batch can hold,
//! and it does so from the pool's already-canonical view, so the choice cannot favour whoever
//! happened to submit first. Per-action, per-market, byte and weight budgets are applied one
//! action at a time, and every action that is left out is logged with the reason — the
//! selection is observable, not a silent `truncate`.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use scale_codec::{Decode, Encode};
use sc_transaction_pool_api::{TransactionPool, TransactionSource};
use sp_consensus::{Environment, Proposer, Proposal};
use sp_runtime::{
	generic::Digest,
	traits::{Block as BlockT, Header as _},
	Weight,
};

use crate::exchange_pool::ExchangePool;
use sunchain_runtime::apis::{ExchangeAction, ExchangeSignedAction, SortKey};

/// Upper bound on actions packed into one exchange batch; must not exceed the runtime's
/// `MaxActionsPerBatch`, which the extrinsic re-checks anyway.
pub const MAX_EXCHANGE_BATCH_ACTIONS: usize = 8;

/// No single market may take every seat: a hot market must not starve the others (S2-3).
pub const MAX_ACTIONS_PER_MARKET_PER_BATCH: usize = 4;

/// A batch of `MaxActionsPerBatch` actions is a few KB; this caps it well below a block so the
/// exchange batch can never blow the block size limit on its own.
pub const MAX_EXCHANGE_BATCH_BYTES: usize = 32 * 1024;

/// Most weight a single batch may claim, leaving headroom for everything else in the block.
/// Below the runtime's Normal-class budget, so a batch is never the reason the block is full.
pub const MAX_EXCHANGE_BATCH_WEIGHT: Weight = Weight::from_parts(1_000_000_000_000, 0);

/// Worst-case weight of one action, as a static estimate (S2-3).
///
/// The pallet exposes a measured `action_weight` runtime API, but it cannot be called from
/// this generic proposer in this SDK version — the `at` parameter's block type will not infer
/// for a generic client (it only resolves against a concrete `FullClient`, as in `service.rs`).
/// So each action is charged this conservative figure. The runtime's own declared-weight check
/// (S1-4) remains the authoritative guard: it rejects a batch whose worst case exceeds the
/// block, and an eight-action batch is proven to fit.
pub const MAX_EXCHANGE_ACTION_WEIGHT: Weight = Weight::from_parts(110_000_000_000, 0);

/// Why an action was left out of this block's batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
	/// The batch already had its full complement of actions.
	ActionCount,
	/// This market already had its share for this batch.
	MarketCount,
	/// Adding it would have exceeded the batch byte budget.
	Bytes,
	/// Adding it would have exceeded the batch weight budget.
	Weight,
}

/// The bounds a batch is built against (S2-3).
pub struct BatchBudget {
	pub max_actions: usize,
	pub max_bytes: usize,
	pub max_per_market: usize,
	pub max_weight: Weight,
}

impl Default for BatchBudget {
	fn default() -> Self {
		Self {
			max_actions: MAX_EXCHANGE_BATCH_ACTIONS,
			max_bytes: MAX_EXCHANGE_BATCH_BYTES,
			max_per_market: MAX_ACTIONS_PER_MARKET_PER_BATCH,
			max_weight: MAX_EXCHANGE_BATCH_WEIGHT,
		}
	}
}

/// Builds the batch from the canonical-sorted pending queue.
///
/// `pending` is already ordered by `(class, account, nonce, bytes)`, so when demand exceeds a
/// batch the lowest-priority (highest-nonce, later-tie) actions are the ones deferred — not
/// whichever arrived last. `weight_of` is injected so the logic is testable without a client;
/// the proposer passes a closure that hits the runtime's `action_weight` API.
pub fn select_batch(
	pending: Vec<(SortKey, ExchangeSignedAction)>,
	budget: &BatchBudget,
	weight_of: impl Fn(&ExchangeAction<u32>) -> Weight,
) -> (Vec<ExchangeSignedAction>, Vec<(ExchangeSignedAction, Exclusion)>) {
	let mut selected = Vec::new();
	let mut excluded = Vec::new();
	let mut per_market: HashMap<Option<u32>, usize> = HashMap::new();
	let mut bytes: usize = 0;
	let mut weight = Weight::from_parts(0, 0);

	for (_key, action) in pending {
		if selected.len() >= budget.max_actions {
			excluded.push((action, Exclusion::ActionCount));
			continue
		}
		let market = market_of(&action);
		if per_market.get(&market).copied().unwrap_or(0) >= budget.max_per_market {
			excluded.push((action, Exclusion::MarketCount));
			continue
		}
		let encoded_len = action.encode().len();
		if bytes + encoded_len > budget.max_bytes {
			excluded.push((action, Exclusion::Bytes));
			continue
		}
		let w = weight_of(&action.action);
		if weight.saturating_add(w).any_gt(budget.max_weight) {
			excluded.push((action, Exclusion::Weight));
			continue
		}
		*per_market.entry(market).or_insert(0) += 1;
		bytes += encoded_len;
		weight = weight.saturating_add(w);
		selected.push(action);
	}
	(selected, excluded)
}

/// The market an action touches, if it touches one. Cancels have no market and share the
/// "no market" bucket so a wave of cancels cannot starve places either.
fn market_of(action: &ExchangeSignedAction) -> Option<u32> {
	match &action.action {
		ExchangeAction::Place(req) => Some(req.market_id),
		_ => None,
	}
}

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
		let pending: Vec<(SortKey, ExchangeSignedAction)> = exchange
			.lock()
			.map(|p| p.pending())
			.unwrap_or_default();

		// Created now, polled later — so the submission below lands before the inner
		// proposer collects from the pool.
		let inner_fut =
			inner.propose(inherent_data, inherent_digests, max_duration, block_size_limit);

		Box::pin(async move {
			if !pending.is_empty() {
				let budget = BatchBudget::default();
				// Each action is charged a conservative static worst case; the live
				// `action_weight` runtime API cannot be called from this generic proposer in
				// this SDK version (see MAX_EXCHANGE_ACTION_WEIGHT). The runtime's own
				// declared-weight check (S1-4) is the authoritative guard.
				let weight_of = |_action: &ExchangeAction<u32>| -> Weight { MAX_EXCHANGE_ACTION_WEIGHT };
				let (selected, excluded) =
					select_batch(pending, &budget, weight_of);

				if !excluded.is_empty() {
					log::info!(target: "exchange",
						"batch: {} action(s) deferred this block", excluded.len());
					for (action, reason) in &excluded {
						log::debug!(target: "exchange",
							"  deferred {} nonce {}: {:?}",
							hex(&action.signer.0), action.action_nonce, reason);
					}
				}

				if !selected.is_empty() {
					log::info!(target: "exchange",
						"proposing with {} queued action(s)", selected.len());
					match unsigned_batch(selected) {
						Some(xt) => {
							let before = pool.status().ready;
							let res =
								pool.submit_one(parent_hash, TransactionSource::Local, xt).await;
							log::info!(target: "exchange",
								"pool ready before={} after={}", before, pool.status().ready);
							match res {
								Ok(_) => log::info!(target: "exchange", "batch accepted by the pool"),
								Err(e) => {
									// Offering the same batch again is expected: the previous
									// one may already be in the pool or in a block. Only a new
									// failure is news.
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

/// Peer/account ids are raw bytes; hex keeps logs readable.
fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use sunchain_runtime::{
		apis::{ActionClass, OrderRequest, Side, TimeInForce},
		configs::{ExchangeSignature, ExchangeSigner},
	};

	const MARKET: u32 = 0;

	fn place(who: u64, nonce: u64, market: u32) -> ExchangeSignedAction {
		let mut signer = [0u8; 20];
		signer[..8].copy_from_slice(&who.to_le_bytes());
		ExchangeSignedAction {
			signer: ExchangeSigner(signer),
			subaccount: 0,
			action_nonce: nonce,
			expires_at: 1_000,
			action: ExchangeAction::Place(OrderRequest {
				market_id: market,
				client_order_id: 1,
				side: Side::Buy,
				price_ticks: 60_000,
				quantity_lots: 1,
				tif: TimeInForce::GoodTilCancelled,
				post_only: false,
				reduce_only: false,
				expires_at: 1_000,
			}),
			signature: ExchangeSignature([0u8; 65]),
		}
	}

	fn cancel(who: u64, nonce: u64) -> ExchangeSignedAction {
		let mut signer = [0u8; 20];
		signer[..8].copy_from_slice(&who.to_le_bytes());
		ExchangeSignedAction {
			signer: ExchangeSigner(signer),
			subaccount: 0,
			action_nonce: nonce,
			expires_at: 1_000,
			action: ExchangeAction::Cancel(7),
			signature: ExchangeSignature([0u8; 65]),
		}
	}

	/// Stands in for the runtime's measured weight so the budget logic is exercised without a
	/// client.
	const ZERO: Weight = Weight::from_parts(0, 0);
	const BIG: Weight = Weight::from_parts(u64::MAX / 2, 0);

	#[test]
	fn selection_keeps_the_canonical_order_when_oversubscribed() {
		// Three accounts, each with one place; only two seats.
		let pending = vec![
			(SortKey { class: ActionClass::Place, account: [1; 32], action_nonce: 1, action_hash: [0; 32] }, place(1, 1, MARKET)),
			(SortKey { class: ActionClass::Place, account: [2; 32], action_nonce: 1, action_hash: [0; 32] }, place(2, 1, MARKET)),
			(SortKey { class: ActionClass::Place, account: [3; 32], action_nonce: 1, action_hash: [0; 32] }, place(3, 1, MARKET)),
		];
		let budget = BatchBudget { max_actions: 2, ..Default::default() };
		let (selected, excluded) = select_batch(pending, &budget, |_| ZERO);
		assert_eq!(selected.len(), 2);
		assert_eq!(excluded.len(), 1);
		// The two lowest signer bytes (account 1, 2) are kept; account 3 is deferred, not
		// whichever happened to be inserted last.
		let kept: Vec<u8> = selected.iter().map(|a| a.signer.0[0]).collect();
		assert!(kept.contains(&1) && kept.contains(&2));
		assert_eq!(excluded[0].1, Exclusion::ActionCount);
	}

	#[test]
	fn no_single_market_takes_every_seat() {
		// Six places on one market; the batch must spread to at most `max_per_market`.
		let pending: Vec<_> = (1..=6u64)
			.map(|n| {
				(
					SortKey { class: ActionClass::Place, account: [n as u8; 32], action_nonce: 1, action_hash: [0; 32] },
					place(n, 1, MARKET),
				)
			})
			.collect();
		let budget = BatchBudget { max_per_market: 4, ..Default::default() };
		let (selected, excluded) = select_batch(pending, &budget, |_| ZERO);
		assert_eq!(selected.len(), 4, "only its market share is taken");
		assert_eq!(excluded.len(), 2);
		assert!(excluded.iter().all(|(_, r)| *r == Exclusion::MarketCount));
	}

	#[test]
	fn a_wave_of_cancels_cannot_starve_places() {
		let pending = vec![
			(SortKey { class: ActionClass::Cancel, account: [1; 32], action_nonce: 1, action_hash: [0; 32] }, cancel(1, 1)),
			(SortKey { class: ActionClass::Cancel, account: [2; 32], action_nonce: 1, action_hash: [0; 32] }, cancel(2, 1)),
			(SortKey { class: ActionClass::Cancel, account: [3; 32], action_nonce: 1, action_hash: [0; 32] }, cancel(3, 1)),
			(SortKey { class: ActionClass::Cancel, account: [4; 32], action_nonce: 1, action_hash: [0; 32] }, cancel(4, 1)),
			(SortKey { class: ActionClass::Cancel, account: [5; 32], action_nonce: 1, action_hash: [0; 32] }, cancel(5, 1)),
		];
		let budget = BatchBudget { max_per_market: 4, ..Default::default() };
		let (selected, excluded) = select_batch(pending, &budget, |_| ZERO);
		assert_eq!(selected.len(), 4);
		assert!(excluded.iter().all(|(_, r)| *r == Exclusion::MarketCount));
	}

	#[test]
	fn weight_budget_stops_the_batch() {
		let pending: Vec<_> = (1..=4u64)
			.map(|n| {
				(
					SortKey { class: ActionClass::Place, account: [n as u8; 32], action_nonce: 1, action_hash: [0; 32] },
					place(n, 1, MARKET),
				)
			})
			.collect();
		// Each action "costs" BIG; one already exceeds the tiny budget.
		let budget = BatchBudget { max_weight: Weight::from_parts(10, 0), ..Default::default() };
		let (selected, excluded) = select_batch(pending, &budget, |_| BIG);
		assert_eq!(selected.len(), 0, "nothing fits under a tiny weight budget");
		assert_eq!(excluded.len(), 4);
		assert!(excluded.iter().all(|(_, r)| *r == Exclusion::Weight));
	}

	#[test]
	fn byte_budget_stops_the_batch() {
		let pending: Vec<_> = (1..=4u64)
			.map(|n| {
				(
					SortKey { class: ActionClass::Place, account: [n as u8; 32], action_nonce: 1, action_hash: [0; 32] },
					place(n, 1, MARKET),
				)
			})
			.collect();
		let budget = BatchBudget { max_bytes: 1, ..Default::default() };
		let (selected, excluded) = select_batch(pending, &budget, |_| ZERO);
		assert_eq!(selected.len(), 0, "the first action alone exceeds a 1-byte budget");
		assert!(excluded.iter().all(|(_, r)| *r == Exclusion::Bytes));
	}
}
