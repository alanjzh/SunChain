//! Premium votes, injected by whoever proposes (dYdX v4 `MsgAddPremiumVotes`).
//!
//! The funding premium is `(impact − index) / index`, and the index price only exists once
//! per sample epoch if nobody samples it in between. dYdX has each proposer vote the premium
//! every block and takes the **median** of the votes at the epoch boundary, which turns one
//! observation per epoch into many.
//!
//! Why a vote rather than computing it inside `on_initialize`: the premium needs an *impact*
//! price, which means walking the order book. Doing that every block would be far too
//! expensive, and `on_initialize` deliberately only runs at the epoch boundaries. The vote
//! moves that cost off-chain.
//!
//! A market with no premium to vote (thin book, no index price) is simply left out.

use sunchain_runtime::apis::ExchangeRuntimeApi as _;
use pallet_exchange_core::Call as ExchangeCall;
use scale_codec::{Decode, Encode};
use sc_client_api::{Backend, BlockchainEvents, HeaderBackend, StorageProvider};
use sc_transaction_pool_api::{TransactionPool, TransactionSource as PoolSource};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::Backend as _;
use sp_runtime::traits::Block as BlockT;
use std::sync::Arc;

/// Markets to vote on.
pub struct PremiumConfig {
	pub markets: Vec<u32>,
}

/// The last premium-vote nonce the chain accepted for this market.
///
/// Read from the chain rather than counted locally, for the same reason as the oracle feed:
/// a restart must not restart the count, and a vote accepted while this node was down must
/// not be replayable.
///
/// The nonce is not decoration. A vote carries nothing else that varies between blocks, so
/// the same premium encodes to the same extrinsic, which is the same hash — and the pool
/// bans the hash of anything it finds in an imported block. Voting without one therefore
/// bans the first successful vote permanently, and every later vote for that market comes
/// back `TemporarilyBanned` for as long as the premium does not change.
fn current_nonce<B, BE, C>(client: &C, at: B::Hash, market_id: u32) -> Option<u64>
where
	B: BlockT,
	BE: Backend<B>,
	C: StorageProvider<B, BE>,
{
	// `Twox64Concat` over the market id, on top of the pallet/item prefix.
	let mut key = sp_core::hashing::twox_128(b"ExchangeCore").to_vec();
	key.extend_from_slice(&sp_core::hashing::twox_128(b"PremiumVoteNonce"));
	let market = market_id.encode();
	key.extend_from_slice(&sp_core::hashing::twox_64(&market));
	key.extend_from_slice(&market);
	// `ValueQuery`, so an absent entry means "never voted" rather than an error.
	let raw = client.storage(at, &sp_core::storage::StorageKey(key)).ok()??;
	u64::decode(&mut &raw.0[..]).ok()
}

/// Reads the premium of every configured market from the local view and votes it, once per
/// imported block.
pub async fn run<B, BE, C, P>(client: Arc<C>, pool: Arc<P>, config: PremiumConfig)
where
	B: BlockT,
	BE: Backend<B>,
	C: BlockchainEvents<B>
		+ HeaderBackend<B>
		+ ProvideRuntimeApi<B>
		+ StorageProvider<B, BE>
		+ Send
		+ Sync
		+ 'static,
	C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<B>,
	P: TransactionPool<Block = B> + 'static,
{
	use futures::StreamExt;

	let mut notifications = client.import_notification_stream();

	while let Some(notification) = notifications.next().await {
		let at = notification.hash;
		let api = client.runtime_api();

		let mut votes = Vec::new();
		for market_id in &config.markets {
			let premium = match api.premium_ppm(at, *market_id) {
				Ok(Some(p)) => p,
				// Too thin to say anything, or no index price yet. Voting zero here would be
				// a lie: it would drag the median toward "no premium".
				Ok(None) => continue,
				Err(e) => {
					log::warn!(target: "exchange", "premium for {market_id} unavailable: {e}");
					continue
				},
			};
			votes.push((*market_id, premium));
		}

		if votes.is_empty() {
			continue
		}

		for (market_id, premium) in votes {
			let nonce = current_nonce::<B, BE, C>(client.as_ref(), at, market_id)
				.unwrap_or(0)
				.saturating_add(1);
			let call = sunchain_runtime::RuntimeCall::ExchangeCore(
				ExchangeCall::submit_premium_votes {
					market_id,
					votes: vec![premium].try_into().expect("one vote fits"),
					nonce,
				},
			);
			let xt = sunchain_runtime::UncheckedExtrinsic::new_bare(call);
			let encoded = xt.encode();
			let Ok(opaque) = B::Extrinsic::decode(&mut &encoded[..]) else {
				continue
			};
			match pool.submit_one(at, PoolSource::Local, opaque).await {
				Ok(_) =>
					log::info!(target: "exchange", "voted premium {premium} ppm for market {market_id}"),
				Err(e) => log::warn!(target: "exchange", "premium vote rejected: {e}"),
			}
		}
	}
}
