//! Dedicated P2P propagation of signed exchange actions (P2-D §12.1 step ②).
//!
//! Exchange actions are not extrinsics, so the standard transaction gossip does not carry
//! them. This is a small flood-fill protocol of its own: every action is verified before it
//! is accepted locally, and accepted actions are relayed once.
//!
//! Four things keep it from being a DoS vector, and all four are pure bookkeeping so they can
//! be tested without a network:
//!
//! * **Size** – a packet larger than a whole batch is dropped before it is decoded; decoding
//!   is the cost an attacker is trying to make us pay.
//! * **Dedup** – an action already seen is never validated, inserted or relayed again. Only
//!   packets that decode as actions occupy a dedup slot, so junk cannot evict real ones.
//! * **Backpressure** – the seen-set and the queue are bounded; growth is capped, not hoped
//!   against.
//! * **Rate limit** – per `(peer, account)` actions, per-peer bytes and per-peer malformed
//!   packets, per window — and the window is advanced by **block height**, never by the
//!   traffic it is supposed to be limiting.

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
use sp_runtime::traits::{Block as BlockT, UniqueSaturatedInto};

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

/// One action is identified by who sent it and which slot they are filling — not by its bytes.
/// Two different payloads for the same `(signer, subaccount, nonce)` are the same action (a
/// re-sign), and a re-sign must not be relayed twice; conversely an action re-relayed by the
/// network arrives with identical bytes and need not be decoded to recognise it.
pub type SeenKey = (Vec<u8>, u32, u64);

/// Bounded set of already-seen action ids, evicting the oldest when full.
pub struct Seen {
	set: HashSet<SeenKey>,
	order: VecDeque<SeenKey>,
	cap: usize,
}

impl Seen {
	pub fn new(cap: usize) -> Self {
		Self { set: HashSet::new(), order: VecDeque::new(), cap }
	}

	/// Returns `true` if this id was **not** seen before.
	pub fn insert(&mut self, id: SeenKey) -> bool {
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

/// A batch is at most `MaxActionsPerBatch` actions, each well under 1 KiB. Anything larger is
/// not an action, and paying to decode it is exactly the cost an attacker is after — so the
/// size check comes before the decoder, and before the dedup set.
pub const MAX_NOTIFICATION_BYTES: usize = 8 * 1024;

/// Blocks per rate-limit window. At 3 s a block that is ~30 s.
pub const RATE_LIMIT_WINDOW_BLOCKS: u32 = 10;

/// Actions one account may have accepted per window, per peer. Twenty per half minute is
/// generous for an order book and, unlike the previous numbers, actually reachable.
pub const MAX_ACTIONS_PER_WINDOW: u32 = 20;

/// Bytes one peer may have accepted per window.
pub const MAX_BYTES_PER_WINDOW: u64 = 256 * 1024;

/// Malformed packets — oversized or undecodable — one peer may send per window.
///
/// Counted separately from the action budget on purpose: junk must not be free, but it must
/// not be able to spend an honest account's budget either.
pub const MAX_MALFORMED_PER_WINDOW: u32 = 32;

/// A packet is only worth decoding if it could be an action (S2-1).
///
/// Checked before the decoder, and before the dedup set: the decode is the cost, and a flood
/// of large packets must not be able to evict real action ids.
pub fn is_action_sized(notification: &[u8]) -> bool {
	notification.len() <= MAX_NOTIFICATION_BYTES
}

/// What one peer has spent in the current window.
#[derive(Default)]
struct PeerUsage {
	bytes: u64,
	malformed: u32,
}

/// Per-`(peer, account)` budget per window, reset by block height.
///
/// Two things were wrong with the previous version, and both made it decorative:
///
/// 1. The window advanced on *messages received* — an attacker could send enough traffic to
///    roll it over and clear every counter, including their own, which is the one they wanted
///    cleared. It now follows the chain's block height, which nobody outside consensus moves.
/// 2. The budget (100) exceeded the window (60 messages), so `allow` could never refuse. The
///    numbers are now reachable by construction.
///
/// A stopped chain stops the window too: with no new blocks nothing resets. That is the safe
/// direction — the alternative is a limiter the traffic it limits can roll over.
pub struct RateLimiter {
	/// `(peer, account) -> actions accepted in the current window`.
	counts: HashMap<(Vec<u8>, Vec<u8>), u32>,
	/// `peer -> bytes and malformed packets in the current window`.
	peers: HashMap<Vec<u8>, PeerUsage>,
	/// Which window the counters belong to, as `block_height / RATE_LIMIT_WINDOW_BLOCKS`.
	window: u32,
}

impl RateLimiter {
	pub fn new() -> Self {
		Self { counts: HashMap::new(), peers: HashMap::new(), window: 0 }
	}

	/// Rolls the counters over when the chain has moved into a new window.
	///
	/// Called with the current block height, once per notification. One comparison in the
	/// common case, so it is not something to avoid calling.
	pub fn advance(&mut self, block_number: u32) {
		let window = block_number / RATE_LIMIT_WINDOW_BLOCKS;
		if window != self.window {
			self.window = window;
			self.counts.clear();
			self.peers.clear();
		}
	}

	/// Charges `bytes` to a peer, and a malformed packet when `malformed`.
	///
	/// Returns `false` once the peer is over either budget. Bytes are charged for every packet,
	/// malformed or not: a peer's bandwidth is spent whether or not the packet was useful.
	pub fn charge(&mut self, peer: &[u8], bytes: u64, malformed: bool) -> bool {
		let usage = self.peers.entry(peer.to_vec()).or_default();
		usage.bytes = usage.bytes.saturating_add(bytes);
		if malformed {
			usage.malformed = usage.malformed.saturating_add(1);
		}
		usage.bytes <= MAX_BYTES_PER_WINDOW && usage.malformed <= MAX_MALFORMED_PER_WINDOW
	}

	/// Charges one accepted action to `(peer, account)`.
	pub fn allow(&mut self, peer: &[u8], account: &[u8]) -> bool {
		let used = self.counts.entry((peer.to_vec(), account.to_vec())).or_insert(0);
		if *used >= MAX_ACTIONS_PER_WINDOW {
			return false
		}
		*used += 1;
		true
	}
}

impl Default for RateLimiter {
	fn default() -> Self {
		Self::new()
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
	let mut limits = RateLimiter::new();

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
			// A locally submitted action: put it on the wire. Deduped by the same semantic key
			// as received actions, so a local submit that was also relayed back to us is not
			// bounced around a second time.
			Which::Local(Some(bytes)) => {
				let relay_once = match scale_codec::Decode::decode(&mut &bytes[..])
					as Result<sunchain_runtime::apis::ExchangeSignedAction, _>
				{
					Ok(action) => seen.insert((
						action.signer.0.to_vec(),
						action.subaccount,
						action.action_nonce,
					)),
					Err(_) => true, // malformed local submission: relay once, peers drop it
				};
				if relay_once {
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
					let peer = peer.to_bytes();
					// Block height, not a tick of our own: the window must not be something the
					// traffic it limits can roll over.
					let block_number = client.info().best_number.unique_saturated_into();
					limits.advance(block_number);

					// Size first, before the decoder and before the dedup set. A packet bigger
					// than a whole batch cannot be one action, and a flood of them must not be
					// able to evict real action ids from `seen`.
					let oversized = !is_action_sized(&notification);
					if !limits.charge(&peer, notification.len() as u64, oversized) {
						continue
					}
					if oversized {
						continue
					}

					let Some(action): Option<sunchain_runtime::apis::ExchangeSignedAction> =
						scale_codec::Decode::decode(&mut &notification[..]).ok()
					else {
						// Not an action, so it must not occupy a dedup slot — but the peer is
						// charged a malformed packet for it.
						limits.charge(&peer, 0, true);
						continue
					};

					// Dedup on the action itself, not its bytes: the same (signer, subaccount,
					// nonce) re-signed is the same action and must not be relayed twice. Only
					// reached by packets that decoded, so junk cannot fill the set.
					if !seen.insert((action.signer.0.to_vec(), action.subaccount, action.action_nonce)) {
						continue
					}

					let accepted = accept(&client, &pool, &mut limits, &peer, &action);
					log::info!(target: "exchange",
						"gossip: {} from {}", if accepted { "accepted" } else { "rejected" },
						hex(&peer));

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
	peer: &[u8],
	action: &sunchain_runtime::apis::ExchangeSignedAction,
) -> bool
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B> + Send + Sync + 'static,
	C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<B>,
{
	// Per `(peer, account)`: one account flooding through one peer is capped, and so is one
	// account spreading itself over many peers — each pairing has its own budget.
	if !limits.allow(peer, &action.signer.0) {
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

/// Peer ids are 32-byte libp2p keys; hex keeps the log readable.
fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn seen_rejects_a_repeat() {
		let mut s = Seen::new(4);
		assert!(s.insert((b"acct".to_vec(), 0u32, 1u64)));
		// Same action re-signed with different bytes is still the same action.
		assert!(!s.insert((b"acct".to_vec(), 0u32, 1u64)), "a repeat must not pass as new");
		assert!(s.insert((b"acct".to_vec(), 0u32, 2u64)));
		// A different signer with the same numbers is a different action.
		assert!(s.insert((b"other".to_vec(), 0u32, 1u64)));
	}

	#[test]
	fn seen_stays_bounded() {
		let mut s = Seen::new(2);
		for n in 1..=4u64 {
			s.insert((b"acct".to_vec(), 0u32, n));
		}
		assert_eq!(s.set.len(), 2, "the oldest ids are evicted, not accumulated");
	}

	/// The dedup key is the action, not its bytes (S2-2).
	///
	/// A client that re-signs the same (signer, subaccount, nonce) with a different payload
	/// produces different bytes but must still be recognised as a repeat. Byte-keyed dedup
	/// would have relayed it a second time.
	#[test]
	fn a_re_sign_is_a_repeat_even_if_the_bytes_differ() {
		let mut s = Seen::new(8);
		assert!(s.insert((b"acct".to_vec(), 0u32, 1u64)));
		assert!(!s.insert((b"acct".to_vec(), 0u32, 1u64)));
		assert!(s.insert((b"acct".to_vec(), 7u32, 1u64)), "a different sub-account is new");
		assert!(s.insert((b"acct".to_vec(), 0u32, 9u64)), "a different nonce is new");
	}

	#[test]
	fn rate_limiter_blocks_past_the_budget() {
		let mut r = RateLimiter::new();
		let (peer, account) = (b"peer".as_slice(), b"acct".as_slice());
		for _ in 0..MAX_ACTIONS_PER_WINDOW {
			assert!(r.allow(peer, account));
		}
		assert!(!r.allow(peer, account), "the next action in the window is refused");
		// A different account, and the same account from a different peer, each have their own.
		assert!(r.allow(peer, b"other"));
		assert!(r.allow(b"elsewhere", account));
	}

	/// The window follows the chain, not the traffic (S2-1).
	///
	/// The old limiter advanced on messages received, so sending more than a window's worth of
	/// traffic rolled it over and cleared every counter — including the attacker's own, which
	/// is the one they wanted cleared. Flooding now changes nothing.
	#[test]
	fn a_flood_cannot_roll_the_window_over() {
		let mut r = RateLimiter::new();
		let (peer, account) = (b"peer".as_slice(), b"acct".as_slice());
		let block_number = 1u32;
		for _ in 0..MAX_ACTIONS_PER_WINDOW {
			assert!(r.allow(peer, account));
		}
		// Far more traffic than any window could hold — a thousand packets at the same block
		// height. The old limiter reset after sixty *messages*, so this is exactly the flood
		// that used to hand an attacker a fresh budget: each packet is a message.
		for _ in 0..1_000 {
			r.advance(block_number);
			r.charge(peer, 1_024, false);
		}
		assert!(
			!r.allow(peer, account),
			"traffic must not be able to reset the budget it is exhausting"
		);
	}

	#[test]
	fn the_window_resets_when_the_chain_moves_on() {
		let mut r = RateLimiter::new();
		let (peer, account) = (b"peer".as_slice(), b"acct".as_slice());
		for _ in 0..MAX_ACTIONS_PER_WINDOW {
			assert!(r.allow(peer, account));
		}

		r.advance(RATE_LIMIT_WINDOW_BLOCKS - 1);
		assert!(!r.allow(peer, account), "still the same window");

		r.advance(RATE_LIMIT_WINDOW_BLOCKS);
		assert!(r.allow(peer, account), "a new window restores the budget");
	}

	#[test]
	fn a_peers_bandwidth_is_bounded() {
		let mut r = RateLimiter::new();
		let peer = b"peer".as_slice();
		let packet = 8 * 1024;
		let mut sent = 0u64;
		while r.charge(peer, packet, false) {
			sent += packet as u64;
			assert!(sent <= MAX_BYTES_PER_WINDOW, "a peer cannot spend more than its budget");
			if sent > MAX_BYTES_PER_WINDOW {
				break
			}
		}
		assert!(sent > 0, "some bandwidth is allowed");
		// The budget is per peer.
		assert!(RateLimiter::new().charge(b"other", packet, false));
	}

	/// Malformed packets are counted, and separately from the action budget (S2-1/S2-2).
	#[test]
	fn malformed_packets_do_not_spend_the_action_budget() {
		let mut r = RateLimiter::new();
		let (peer, account) = (b"peer".as_slice(), b"acct".as_slice());

		// A peer sending nothing but junk: charged as malformed, and eventually cut off.
		for _ in 0..MAX_MALFORMED_PER_WINDOW {
			assert!(r.charge(peer, 0, true));
		}
		assert!(!r.charge(peer, 0, true), "junk has its own ceiling");

		// ...and an honest account's budget is untouched by any of it.
		let mut fresh = RateLimiter::new();
		for _ in 0..MAX_MALFORMED_PER_WINDOW {
			fresh.charge(peer, 0, true);
		}
		assert!(fresh.allow(peer, account), "junk must not spend an honest account's budget");
	}

	/// Only packets that could be an action are worth decoding (S2-1).
	#[test]
	fn only_action_sized_packets_are_worth_decoding() {
		assert!(is_action_sized(&[0u8; MAX_NOTIFICATION_BYTES]));
		assert!(
			!is_action_sized(&[0u8; MAX_NOTIFICATION_BYTES + 1]),
			"a packet larger than a whole batch cannot be one action"
		);
	}

	/// An oversized packet spends the peer's byte budget as well as its malformed one, so
	/// whichever ceiling is reached first cuts the peer off (S2-1).
	#[test]
	fn oversized_packets_spend_the_byte_budget_too() {
		let mut r = RateLimiter::new();
		let mut sent = 0u64;
		while r.charge(b"peer", MAX_NOTIFICATION_BYTES as u64 + 1, true) {
			sent += MAX_NOTIFICATION_BYTES as u64 + 1;
		}
		assert!(sent > 0, "some bandwidth is allowed");
		assert!(
			sent <= MAX_BYTES_PER_WINDOW,
			"the peer is cut off at its byte budget, not after it"
		);
	}
}
