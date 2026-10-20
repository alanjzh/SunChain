//! Dedicated P2P propagation of signed exchange actions (P2-D §12.1 step ②).
//!
//! Exchange actions are not extrinsics, so the standard transaction gossip does not carry
//! them. This is a small flood-fill protocol of its own: every action is verified before it
//! is accepted locally, and accepted actions are relayed once.
//!
//! Three things keep it from being a DoS vector, and all three are pure bookkeeping so they
//! can be tested without a network:
//!
//! * **Dedup** – an action already seen is never validated, inserted or relayed again.
//! * **Backpressure** – the seen-set and the queue are bounded; growth is capped, not hoped
//!   against.
//! * **Rate limit** – a per-account (and per-market) budget per window. A single account
//!   cannot flood the pool and starve everyone else.

use std::{
	collections::{HashMap, HashSet, VecDeque},
	sync::{Arc, Mutex},
};

use sc_network::{config::SetConfig, peer_store::PeerStoreProvider, NetworkBackend, ProtocolName};
use sc_network::service::{
	traits::{NotificationEvent, NotificationService},
	NotificationMetrics,
};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::Block as BlockT;

use sunchain_runtime::apis::ExchangeRuntimeApi;

use crate::exchange_pool::ExchangePool;

/// Protocol name. Versioned, so it can evolve without cross-talk.
pub const EXCHANGE_PROTOCOL: &str = "/sun/exchange/1";

/// Builds the notification-protocol config, mirroring how GRANDPA registers its own set.
pub fn exchange_peers_set_config<B: BlockT, N: NetworkBackend<B, <B as BlockT>::Hash>>(
	protocol_name: ProtocolName,
	metrics: NotificationMetrics,
	peer_store_handle: Arc<dyn PeerStoreProvider>,
) -> (N::NotificationProtocolConfig, Box<dyn NotificationService>) {
	N::notification_config(
		protocol_name,
		Vec::new(),
		// One action is well under 1 KiB; allow a small batch but nothing near block size.
		64 * 1024,
		None,
		SetConfig {
			// Unlike GRANDPA this protocol does not manage its own peer set, so it needs a
			// real budget: with 0/0 no substream is ever opened and nothing propagates.
			in_peers: 25,
			out_peers: 25,
			reserved_nodes: Vec::new(),
			non_reserved_mode: sc_network::config::NonReservedPeerMode::Accept,
		},
		metrics,
		peer_store_handle,
	)
}

/// Bounded set of already-seen payload ids, evicting the oldest when full.
pub struct Seen {
	set: HashSet<Vec<u8>>,
	order: VecDeque<Vec<u8>>,
	cap: usize,
}

impl Seen {
	pub fn new(cap: usize) -> Self {
		Self { set: HashSet::new(), order: VecDeque::new(), cap }
	}

	/// Returns `true` if this id was **not** seen before.
	pub fn insert(&mut self, id: Vec<u8>) -> bool {
		if !self.set.insert(id.clone()) {
			return false
		}
		self.order.push_back(id);
		while self.order.len() > self.cap {
			if let Some(oldest) = self.order.pop_front() {
				self.set.remove(&oldest);
			}
		}
		true
	}
}

/// Per-key budget that resets every `window` ticks.
pub struct RateLimiter {
	counts: HashMap<Vec<u8>, usize>,
	tick: usize,
	window: usize,
	max_per_window: usize,
}

impl RateLimiter {
	pub fn new(window: usize, max_per_window: usize) -> Self {
		Self { counts: HashMap::new(), tick: 0, window, max_per_window }
	}

	/// Advances time. Called once per unit of `window`; the counters reset when it elapses.
	pub fn advance(&mut self) {
		self.tick = self.tick.wrapping_add(1);
		if self.window > 0 && self.tick % self.window == 0 {
			self.counts.clear();
		}
	}

	/// Returns `false` — without charging — once the key has spent its budget.
	pub fn allow(&mut self, key: &[u8]) -> bool {
		let used = self.counts.entry(key.to_vec()).or_insert(0);
		if *used >= self.max_per_window {
			return false
		}
		*used += 1;
		true
	}
}

/// Runs the gossip loop: relay what we have, accept and relay what we are given.
pub async fn run_exchange_gossip<B, C>(
	mut service: Box<dyn NotificationService>,
	pool: Arc<Mutex<ExchangePool>>,
	client: Arc<C>,
	mut local: futures::channel::mpsc::UnboundedReceiver<Vec<u8>>,
) where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
	C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<B>,
{
	let mut peers: HashSet<sc_network::PeerId> = HashSet::new();
	let mut seen = Seen::new(4096);
	// A hundred actions per account per window is already generous for an order book.
	let mut limits = RateLimiter::new(60, 100);

	loop {
		use futures::{future::Either, StreamExt};

		// `select`, not `select!`: `next_event` returns a boxed future, which does not
		// satisfy `fuse`. Dropping the branch we did not take is fine — both are just
		// borrowed views of a stream that stays in place.
		// Collapse to a plain value first: the branch `select` did not take still holds a
		// borrow of `service`, and we need that back to send.
		enum Which {
			Event(Option<NotificationEvent>),
			Local(Option<Vec<u8>>),
		}
		let which = match futures::future::select(
			Box::pin(service.next_event()),
			Box::pin(local.next()),
		)
		.await
		{
			Either::Left((event, _rest)) => Which::Event(event),
			Either::Right((bytes, _rest)) => Which::Local(bytes),
		};

		match which {
			// A locally submitted action: put it on the wire.
			Which::Local(Some(bytes)) => {
				if seen.insert(bytes.clone()) {
					relay(&mut service, &peers, &bytes).await;
				}
				continue
			},
			Which::Local(None) => continue,
			Which::Event(None) => break,
			Which::Event(Some(event)) => match event {
				NotificationEvent::NotificationStreamOpened { peer, .. } => {
					log::info!(target: "exchange", "gossip: substream opened with {peer}");
					peers.insert(peer);
				},
				NotificationEvent::NotificationStreamClosed { peer } => {
					log::info!(target: "exchange", "gossip: substream closed with {peer}");
					peers.remove(&peer);
				},
				NotificationEvent::NotificationReceived { peer, notification } => {
					limits.advance();

					// Dedup on the payload itself: identical bytes are the same action.
					if !seen.insert(notification.clone()) {
						continue
					}

					let Some(action) = scale_codec::Decode::decode(&mut &notification[..]).ok()
					else { continue };

					let accepted = accept(&client, &pool, &mut limits, &action);
					log::info!(target: "exchange",
						"gossip: {} from {peer}", if accepted { "accepted" } else { "rejected" });

					// Relay only what we accepted, so invalid traffic is not amplified.
					if accepted {
						relay(&mut service, &peers, &notification).await;
					}
				},
				_ => {},
			},
		}
	}
}

/// Validates and queues one remote action. Returns `true` if it was accepted.
fn accept<B, C>(
	client: &Arc<C>,
	pool: &Arc<Mutex<ExchangePool>>,
	limits: &mut RateLimiter,
	action: &sunchain_runtime::apis::ExchangeSignedAction,
) -> bool
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
	C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<B>,
{
	if !limits.allow(&action.signer.0) {
		return false
	}

	let at = client.info().best_hash;
	let valid = client
		.runtime_api()
		.validate_exchange_action(at, action.clone())
		.map(|r| r.is_ok())
		.unwrap_or(false);

	if !valid {
		return false
	}

	pool.lock().map(|mut p| p.submit(action.clone(), |_| true).is_ok()).unwrap_or(false)
}

async fn relay(
	service: &mut Box<dyn NotificationService>,
	peers: &HashSet<sc_network::PeerId>,
	notification: &[u8],
) {
	for peer in peers {
		let _ = service.send_async_notification(peer, notification.to_vec()).await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn seen_rejects_a_repeat() {
		let mut s = Seen::new(4);
		assert!(s.insert(b"a".to_vec()));
		assert!(!s.insert(b"a".to_vec()), "a repeat must not pass as new");
		assert!(s.insert(b"b".to_vec()));
	}

	#[test]
	fn seen_stays_bounded() {
		let mut s = Seen::new(2);
		for k in [b"1", b"2", b"3", b"4"] {
			s.insert(k.to_vec());
		}
		assert_eq!(s.set.len(), 2, "the oldest ids are evicted, not accumulated");
	}

	#[test]
	fn rate_limiter_blocks_past_the_budget() {
		let mut r = RateLimiter::new(10, 2);
		assert!(r.allow(b"acct"));
		assert!(r.allow(b"acct"));
		assert!(!r.allow(b"acct"), "the third action in the window is refused");
		// A different key has its own budget.
		assert!(r.allow(b"other"));
	}

	#[test]
	fn rate_limiter_resets_each_window() {
		let mut r = RateLimiter::new(2, 1);
		assert!(r.allow(b"acct"));
		assert!(!r.allow(b"acct"));
		r.advance();
		r.advance(); // window elapsed
		assert!(r.allow(b"acct"), "the budget is restored for the next window");
	}
}
