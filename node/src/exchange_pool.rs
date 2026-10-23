//! Dedicated mempool for signed exchange actions (P2-D §12.1 step ①).
//!
//! Exchange actions are deliberately *not* Substrate extrinsics. They are submitted by
//! users, held here, and later bundled into a single `execute_exchange_batch` by the
//! proposer (§12.4). Keeping them out of the general pool means a burst of orders cannot
//! crowd out governance, staking or EVM traffic.
//!
//! Only bookkeeping lives here — validation is injected by the caller — so the ordering
//! rules can be tested without a client.

use std::collections::BTreeMap;

use sunchain_runtime::apis::{ExchangeSignedAction, SortKey};

/// Why the pool refused an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
	/// Runtime pre-validation (§12.2) refused it — bad signature or a stale nonce.
	Invalid,
	/// The pool is at its limit.
	Full,
	/// The same `(signer, subaccount, nonce)` is already queued.
	Duplicate,
}

/// Actions waiting to be bundled.
pub struct ExchangePool {
	queue: BTreeMap<(Vec<u8>, u32, u64), ExchangeSignedAction>,
	limit: usize,
}

impl ExchangePool {
	pub fn new(limit: usize) -> Self {
		Self { queue: BTreeMap::new(), limit }
	}

	/// Queues an action, rejecting duplicates and refusing to grow without bound.
	///
	/// `validate` is the runtime pre-check of §12.2. It is **advisory**: the runtime replay
	/// inside `execute_exchange_batch` is the only authority, so passing here guarantees
	/// nothing. It only keeps obvious junk out of the pool and gives users fast feedback.
	pub fn submit(
		&mut self,
		action: ExchangeSignedAction,
		validate: impl Fn(&ExchangeSignedAction) -> bool,
	) -> Result<(), Reject> {
		if !validate(&action) {
			return Err(Reject::Invalid)
		}
		let key = (action.signer.0.to_vec(), action.subaccount, action.action_nonce);
		if self.queue.contains_key(&key) {
			return Err(Reject::Duplicate)
		}
		if self.queue.len() >= self.limit {
			return Err(Reject::Full)
		}
		self.queue.insert(key, action);
		Ok(())
	}

	/// Queued actions in deterministic execution order (§12.3 / S1-3).
	///
	/// The runtime re-sorts the batch and refuses one that is out of order, so this is not
	/// what makes execution deterministic. It is what makes the *selection* deterministic
	/// when there is more demand than one batch can hold — otherwise whichever actions
	/// happened to arrive first would win, and the proposer could be accused of favouring
	/// them. The key is signed throughout, so the order is the same everywhere.
	pub fn pending(&self) -> Vec<(SortKey, ExchangeSignedAction)> {
		let mut out: Vec<(SortKey, ExchangeSignedAction)> = self
			.queue
			.values()
			.map(|action| (action.sort_key(), action.clone()))
			.collect();
		out.sort_by_key(|(key, _)| *key);
		out
	}

	/// Drops every action the predicate rejects.
	///
	/// The proposer never removes anything: an action stays queued until this says it is
	/// done — i.e. until the chain has consumed its nonce and re-validation refuses it.
	/// Removing it at propose time instead would silently lose it whenever the block was
	/// discarded or reorganised out.
	pub fn retain_valid(&mut self, is_valid: impl Fn(&ExchangeSignedAction) -> bool) {
		self.queue.retain(|_, action| is_valid(action));
	}

	pub fn len(&self) -> usize {
		self.queue.len()
	}

	pub fn is_empty(&self) -> bool {
		self.queue.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sunchain_runtime::{
		apis::{ActionClass, ExchangeAction, OrderRequest, Side, TimeInForce},
		configs::{ExchangeSignature, ExchangeSigner},
	};

	const MARKET: u32 = 0;

	fn order_request() -> OrderRequest<u32> {
		OrderRequest {
			market_id: MARKET,
			client_order_id: 1,
			side: Side::Buy,
			price_ticks: 60_000,
			quantity_lots: 1,
			tif: TimeInForce::GoodTilCancelled,
			post_only: false,
			reduce_only: false,
			expires_at: 1_000,
		}
	}

	/// `who` is folded into the signer bytes; the signature is inert in these tests
	/// because validation is injected.
	fn action(who: u64, nonce: u64, action: ExchangeAction<u32>) -> ExchangeSignedAction {
		let mut signer = [0u8; 20];
		signer[..8].copy_from_slice(&who.to_le_bytes());
		ExchangeSignedAction {
			signer: ExchangeSigner(signer),
			subaccount: 0,
			action_nonce: nonce,
			expires_at: 1_000,
			action,
			signature: ExchangeSignature([0u8; 65]),
		}
	}

	fn place() -> ExchangeAction<u32> {
		ExchangeAction::Place(order_request())
	}

	fn pool() -> ExchangePool {
		ExchangePool::new(8)
	}

	#[test]
	fn accepts_an_action_that_validates() {
		let mut p = pool();
		assert_eq!(p.submit(action(1, 1, place()), |_| true), Ok(()));
		assert_eq!(p.len(), 1);
	}

	#[test]
	fn rejects_what_the_runtime_precheck_refuses() {
		let mut p = pool();
		assert_eq!(p.submit(action(1, 1, place()), |_| false), Err(Reject::Invalid));
		assert!(p.is_empty());
	}

	#[test]
	fn the_same_nonce_cannot_be_queued_twice() {
		let mut p = pool();
		assert_eq!(p.submit(action(1, 1, place()), |_| true), Ok(()));
		assert_eq!(
			p.submit(action(1, 1, place()), |_| true),
			Err(Reject::Duplicate),
			"a replay must not be able to occupy two slots"
		);
	}

	#[test]
	fn the_pool_is_bounded() {
		let mut p = pool();
		for n in 1..=8 {
			assert_eq!(p.submit(action(1, n, place()), |_| true), Ok(()));
		}
		assert_eq!(p.submit(action(1, 9, place()), |_| true), Err(Reject::Full));
	}

	/// §12.3 / S1-3: cancels before places, then account, then nonce.
	#[test]
	fn pending_is_ordered_by_class_then_account_then_nonce() {
		let mut p = pool();
		// Submitted place-first, so only the sort can put the cancel ahead of it.
		p.submit(action(1, 1, place()), |_| true).unwrap();
		p.submit(action(2, 2, ExchangeAction::Cancel(7)), |_| true).unwrap();

		let ordered = p.pending();
		assert_eq!(ordered[0].0.class, ActionClass::Cancel);
		assert_eq!(ordered[1].0.class, ActionClass::Place);
	}

	/// A tie is broken by the action's own signed bytes, never by who arrived first (S1-3).
	///
	/// The key used to end in the arrival index — the one part of it the pool itself chose,
	/// which is what let a proposer order the batch by submission time. Two accounts with the
	/// same class and nonce now order by their signed bytes, so submitting them the other way
	/// round yields the same sequence.
	#[test]
	fn a_tie_is_broken_by_the_action_not_by_arrival() {
		let first = action(9, 1, place());
		let second = action(5, 1, place());

		let order = |a: ExchangeSignedAction, b: ExchangeSignedAction| {
			let mut p = pool();
			p.submit(a, |_| true).unwrap();
			p.submit(b, |_| true).unwrap();
			p.pending().into_iter().map(|(_, action)| action.signer.0).collect::<Vec<_>>()
		};

		assert_eq!(
			order(first.clone(), second.clone()),
			order(second, first),
			"the sequence must not depend on submission order"
		);
	}

	#[test]
	fn retain_valid_drops_only_what_is_rejected() {
		let mut p = pool();
		p.submit(action(1, 1, place()), |_| true).unwrap();
		p.submit(action(2, 1, place()), |_| true).unwrap();

		// Only the first is still executable; the second has been consumed on chain.
		p.retain_valid(|a| a.signer.0[0] == 1);
		assert_eq!(p.len(), 1, "only the executed action is dropped");
	}
}
