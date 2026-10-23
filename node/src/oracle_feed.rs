//! The price feed daemon (dYdX v4 parity).
//!
//! dYdX runs a *pricefeed daemon* beside every validator: it polls third-party exchanges and
//! publishes the resulting index price on chain, signed by the validator's own key. This is
//! the wiring that makes the oracle actually receive prices.
//!
//! Everything here is deliberately explicit about the two things that are easy to get wrong
//! and impossible to notice afterwards:
//!
//! 1. **Quote currency.** Venues quote the same market in different currencies, so each one
//!    has to be brought onto the market's own quote currency before it can be medianised.
//! 2. **Ticks.** The runtime stores prices as whole ticks, not as an amount of money.
//!
//! Both already have their own unit-tested implementations (`pricefeed::convert`,
//! `price_to_ticks`); this module is only the plumbing between them and the chain.

use crate::pricefeed::{self, ConversionDetails, Exchange, Quote};
use std::collections::{BTreeMap, HashMap};
use sunchain_runtime::{Address, RuntimeCall, SignedPayload, TxExtension, UncheckedExtrinsic};
use ecdsa_keyring::Keyring;
use exchange_primitives::{price_to_ticks, MarketId, PriceTicks, FEED_SCALE};
use sunchain_runtime::apis::ExchangeRuntimeApi as _;
use sc_client_api::{Backend, BlockchainEvents, HeaderBackend, StorageProvider};
use sp_api::ProvideRuntimeApi;
use sc_transaction_pool_api::{TransactionPool, TransactionSource as PoolSource};
use scale_codec::{Decode, Encode};
use sp_runtime::{
	generic::Era,
	traits::Block as BlockT,
	MultiSignature,
};
use std::{sync::Arc, time::Duration};

/// One market the feed publishes, and how to arrive at its price.
pub struct FeedMarket {
	pub market_id: MarketId,
	/// `(venue, the venue's own spelling of the pair, how to bring it onto our currency)`.
	pub sources: Vec<(Exchange, String, ConversionDetails)>,
	/// Venues for the conversion rate itself, e.g. `USDT-USD`.
	pub rate_sources: Vec<(Exchange, String)>,
	/// How many venues must answer for the market.
	pub min_exchanges: usize,
	/// How many venues must answer for the conversion rate.
	pub rate_min_exchanges: usize,
	/// The quote asset's decimals (6 for USDT and USDC). The *tick size* is deliberately not
	/// here: it is read from the chain each round, so the two cannot drift apart.
	pub quote_decimals: u8,
}

pub struct FeedConfig {
	pub interval: Duration,
	/// How long a quote stays fresh enough to count.
	pub max_age: Duration,
	pub markets: Vec<FeedMarket>,
	/// Every whitelisted key this operator holds. A real deployment has one; the dev chain
	/// has two so the quorum can be met at all.
	pub signers: Vec<Keyring>,
}

/// Everything one round fetched, keyed by venue and that venue's spelling of the market.
///
/// A round is a handful of requests, not one per market: the venues are asked once (or in
/// batches) and the markets are picked out of what came back. Collecting per market instead
/// is what made four markets cost 10.6 MB every thirty seconds — the same venue's whole book,
/// downloaded once per market.
pub struct Round {
	quotes: HashMap<(Exchange, String), Quote>,
}

impl Round {
	pub fn get(&self, exchange: Exchange, symbol: &str) -> Option<Quote> {
		self.quotes
			.iter()
			.find(|((venue, got), _)| *venue == exchange && pricefeed::symbol_matches(exchange, got, symbol))
			.map(|(_, quote)| *quote)
	}

	pub fn len(&self) -> usize {
		self.quotes.len()
	}
}

/// Every `(venue, market)` a round has to ask for, deduplicated.
///
/// The peg is configured per market but is the same rate for all of them, so it appears here
/// once however many markets ask for it.
pub fn round_symbols(markets: &[FeedMarket]) -> BTreeMap<Exchange, Vec<String>> {
	let mut wanted: BTreeMap<Exchange, Vec<String>> = BTreeMap::new();
	for market in markets {
		for (exchange, symbol, _) in &market.sources {
			want(&mut wanted, *exchange, symbol)
		}
		for (exchange, symbol) in &market.rate_sources {
			want(&mut wanted, *exchange, symbol)
		}
	}
	wanted
}

fn want(wanted: &mut BTreeMap<Exchange, Vec<String>>, exchange: Exchange, symbol: &str) {
	let list = wanted.entry(exchange).or_default();
	if !list.iter().any(|s| s == symbol) {
		list.push(symbol.to_string())
	}
}

/// Asks each venue once per round for every market it is needed for.
///
/// A venue that cannot be reached contributes nothing, which is the per-venue failure the
/// median already expects.
pub async fn fetch_round(
	client: &reqwest::Client,
	wanted: &BTreeMap<Exchange, Vec<String>>,
) -> Round {
	let mut quotes = HashMap::new();
	for (exchange, symbols) in wanted {
		for (symbol, quote) in pricefeed::fetch_many(client, *exchange, symbols).await {
			quotes.insert((*exchange, symbol), quote);
		}
	}
	Round { quotes }
}

/// Collects the index price of one market, in the market's own quote currency, in ticks.
///
/// Takes the round's quotes rather than the network: the fetching already happened, once per
/// venue, and a market is picked out of it here.
///
/// Returns `None` when there is no trustworthy price — which is the intended outcome far more
/// often than one might like: a venue being down, or the rate being unavailable, must produce
/// *no* price rather than a wrong one.
pub fn collect_from(
	round: &Round,
	market: &FeedMarket,
	tick_size_quote: u128,
	max_age: Duration,
) -> Option<PriceTicks> {
	let now = pricefeed::now_ms();
	let max_age_ms = max_age.as_millis() as u64;

	// The conversion rate first: without it, venues that need converting cannot be used.
	let mut rate_quotes = Vec::new();
	for (exchange, symbol) in &market.rate_sources {
		if let Some(quote) = round.get(*exchange, symbol) {
			rate_quotes.push(quote);
		}
	}
	let rate = pricefeed::index_price(&rate_quotes, market.rate_min_exchanges, now, max_age_ms);
	// Our markets are quoted in the quote asset, so a USD venue needs "quote per USD", which
	// is the reciprocal of the "USD per quote" rate we can actually observe.
	let rate_inverted = rate.map(pricefeed::invert_millionths);

	let mut converted = Vec::new();
	for (exchange, symbol, details) in &market.sources {
		let Some(raw) = round.get(*exchange, symbol) else { continue };
		let Ok(price) = pricefeed::convert(
			raw.price_millionths,
			*details,
			rate_inverted.map(|r| (r, rate_quotes.len())),
		) else {
			continue
		};
		converted.push(price);
	}

	let median = pricefeed::index_price(
		&converted
			.iter()
			.map(|p| pricefeed::Quote {
				exchange: Exchange::Binance,
				price_millionths: *p,
				at_ms: now,
			})
			.collect::<Vec<_>>(),
		market.min_exchanges,
		now,
		max_age_ms,
	)?;

	price_to_ticks(median as u128, FEED_SCALE, market.quote_decimals, tick_size_quote)
}

/// Builds the injected price vote (dYdX v4 `MsgUpdateMarketPrices`).
///
/// Unsigned at the extrinsic level, so no fee, no balance and no queueing — the source
/// authenticates itself inside the call instead. Separate from the loop so it can be checked
/// without a network: a payload mismatch produces a vote that is simply rejected, with
/// nothing to look at.
pub fn build_vote(
	prices: Vec<(MarketId, PriceTicks)>,
	signer: Keyring,
	nonce: u64,
) -> Option<UncheckedExtrinsic> {
	let source = signer.to_account_id();
	// The payload has to be byte-identical to what the pallet recomputes, so it comes from the
	// pallet rather than being rebuilt here.
	let payload = pallet_oracle::Pallet::<sunchain_runtime::Runtime>::vote_payload(
		&source, &prices, nonce,
	);
	// `EthereumSignature` recovers the signer and compares it to the account, so it expects an
	// Ethereum-style signature: a *recoverable* one over `keccak_256`, not Substrate's
	// `blake2_256` digest that `Pair::sign` would produce.
	let digest = sp_io::hashing::keccak_256(&payload);
	let signature =
		sunchain_runtime::Signature::from(MultiSignature::Ecdsa(signer.pair().sign_prehashed(&digest)));

	Some(UncheckedExtrinsic::new_bare(RuntimeCall::Oracle(
		pallet_oracle::Call::submit_price_vote {
			source,
			signature,
			prices: prices.try_into().ok()?,
			nonce,
		},
	)))
}

/// The source's last used nonce, read from the chain.
///
/// An unsigned extrinsic carries no nonce of its own, so the vote has to carry one and it has
/// to be ahead of the last one the chain saw. Reading it rather than counting locally is what
/// makes a restart safe.
pub fn current_nonce<B, BE, C>(
	client: &C,
	at: B::Hash,
	who: sunchain_runtime::AccountId,
) -> Option<u64>
where
	B: BlockT,
	BE: Backend<B>,
	C: StorageProvider<B, BE>,
{
	let mut key = sp_core::hashing::twox_128(b"Oracle").to_vec();
	key.extend_from_slice(&sp_core::hashing::twox_128(b"Sources"));
	let raw = client.storage(at, &sp_core::storage::StorageKey(key)).ok()??;
	let records: Vec<pallet_oracle::SourceRecord<sunchain_runtime::AccountId, u32>> =
		Decode::decode(&mut &raw.0[..]).ok()?;
	records.into_iter().find(|r| r.key == who).map(|r| r.last_nonce)
}

/// Polls the venues and publishes an index price on every tick.
///
/// A market whose price cannot be established is simply left out: no price is the correct
/// answer when the venues disagree, are down, or the conversion rate is missing. Publishing
/// nothing is visible; publishing a wrong price is not.
pub async fn run<B, BE, C, P>(client: Arc<C>, pool: Arc<P>, config: FeedConfig)
where
	B: BlockT,
	BE: Backend<B>,
	C: StorageProvider<B, BE> + HeaderBackend<B> + ProvideRuntimeApi<B> + BlockchainEvents<B>
		+ Send
		+ Sync
		+ 'static,
	C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<B>,
	P: TransactionPool<Block = B> + 'static,
{
	// Log before doing anything else, so "the task never started" can be told apart from
	// "the task started and found nothing".
	log::info!(target: "oracle", "oracle feed task started");

	let http = match reqwest::Client::builder()
		.user_agent("sunchain-oracle-feed")
		.timeout(Duration::from_secs(10))
		.build()
	{
		Ok(c) => c,
		Err(e) => {
			log::error!(target: "oracle", "feed cannot start: {e}");
			return
		},
	};

	// Every market a round needs, per venue, asked for once — not once per market.
	let wanted = round_symbols(&config.markets);
	log::info!(
		target: "oracle",
		"feed started: {} market(s) over {} venue(s), every {:?}",
		config.markets.len(),
		wanted.len(),
		config.interval
	);

	// Driven by block imports rather than `tokio::time::interval`: the timer never fired here,
	// so the feed sat silently doing nothing. Blocks arrive every couple of seconds, which is
	// a fine clock for a feed that publishes every half minute or so.
	use futures::StreamExt;

let mut notifications = client.import_notification_stream();
	let mut last_published: Option<std::time::Instant> = None;
	let mut round = 0u64;

	while notifications.next().await.is_some() {
		let due = match last_published {
			None => true,
			Some(at) => at.elapsed() >= config.interval,
		};
		if !due {
			continue
		}
		round = round.saturating_add(1);
		log::info!(target: "oracle", "round {round}: polling");
		last_published = Some(std::time::Instant::now());

		let at = client.info().best_hash;
		let api = client.runtime_api();

		// One pass over the venues, then the markets are picked out of what came back.
		let quotes = fetch_round(&http, &wanted).await;
		log::info!(target: "oracle", "round {round}: {} quote(s) fetched", quotes.len());

		let mut prices = Vec::new();
		for market in &config.markets {
			// The tick size comes from the chain, not from config: a mismatch here would make
			// every price wrong by orders of magnitude, with nothing to notice it by.
			let Some(tick_size) = api.tick_size_quote(at, market.market_id).ok().flatten() else {
				log::warn!(target: "oracle", "market {} has no tick size on chain", market.market_id);
				continue
			};
			match collect_from(&quotes, market, tick_size, config.max_age) {
				Some(ticks) => prices.push((market.market_id, ticks)),
				None =>
					log::warn!(target: "oracle", "no price for market {}", market.market_id),
			}
		}
		if prices.is_empty() {
			// Never silent: a feed that quietly does nothing is indistinguishable from one
			// that is not running, which is exactly how this went unnoticed.
			log::warn!(target: "oracle", "round {round}: no market produced a price");
			continue
		}

		// One vote per key. A real deployment has one key per operator; the dev chain has one
		// operator holding several, which exercises the quorum but not its independence.
		for signer in &config.signers {
			let source = signer.to_account_id();
			let Some(last) = current_nonce::<B, BE, C>(client.as_ref(), at, source) else {
				log::warn!(target: "oracle", "a feed key is not a whitelisted source; skipped");
				continue
			};
			let Some(vote) = build_vote(prices.clone(), *signer, last.saturating_add(1)) else {
				continue
			};

			// The pool wants the block's own extrinsic type, not the runtime's concrete one.
			let encoded = vote.encode();
			let Ok(opaque) = B::Extrinsic::decode(&mut &encoded[..]) else {
				log::warn!(target: "oracle", "vote does not decode as an extrinsic");
				continue
			};

			match pool.submit_one(at, PoolSource::Local, opaque).await {
				Ok(_) => log::info!(target: "oracle", "published a price vote as {source}"),
				Err(e) => log::warn!(target: "oracle", "vote rejected: {e}"),
			}
		}
	}
}
