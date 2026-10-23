//! Off-chain price feed (dYdX v4 parity).
//!
//! dYdX v4 runs a *pricefeed daemon* beside every validator
//! (`protocol/daemons/pricefeed/`). It polls third-party exchanges; the index price is the
//! **median across venues** — never a mean, so one broken or manipulated venue cannot move
//! it — and it requires at least `MinExchanges` venues to answer at all
//! (`price_encoder.go`). Fewer than that and the daemon produces **no** price rather than
//! a bad one. Venues that went stale are dropped before the median.
//!
//! We mirror exactly that. The result feeds `pallet-oracle`'s `submit_prices`, which then
//! takes a *second* median across its authorised sources (the quorum layer).
//!
//! Reference implementation: `/home/alan/alan/dydx/v4-chain`
//! (see `docs/Sunchain-Phase2-Exchange-Core-Detailed-Design.md`).

use std::time::{SystemTime, UNIX_EPOCH};

/// Exchanges dYdX v4 supports today
/// (`protocol/daemons/pricefeed/client/price_function/`).
///
/// An explicit enum rather than a string, so a typo in config is a compile error and
/// "which venues back our index price" is answerable by reading this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Exchange {
	Binance,
	Bitfinex,
	Bitstamp,
	Bybit,
	CoinbasePro,
	CryptoCom,
	Gate,
	Huobi,
	Kraken,
	KuCoin,
	Mexc,
	Okx,
}

/// How a venue can be asked for several markets at once.
///
/// This decides what a round costs. Getting it wrong is invisible locally and expensive
/// remotely: a venue treated as [`FetchMode::Single`] costs one request *per market per
/// round*, which is what turned a four-market feed into gigabytes a day. Measured 2026-10-23:
///
/// * Binance, one market: 559 B, weight 2. The whole book of 3 739 pairs: **1.9 MB**,
///   weight 82 — and `?symbols=[…]` takes up to ~450 of them in one 233 kB request, after
///   which the gateway answers **414**.
/// * OKX has no batch endpoint at all (`instId=` is ignored, it still returns all 1 186
///   instruments), so the only cheap move is one full request per round: 372 kB, 101 kB
///   gzipped.
/// * Kraken answers a comma-separated `pair=` list, but one unlisted pair fails the *whole*
///   batch, so a batch that came back empty has to be retried market by market.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchMode {
	/// One request returns every market; we pick ours out of it locally.
	All,
	/// One request can carry several symbols. `max` is what we dare put in one URL.
	Batch { max: usize },
	/// One market per request.
	Single,
}

impl Exchange {
	/// Every venue dYdX v4 ships a price function for.
	pub const ALL: [Exchange; 12] = [
		Exchange::Binance,
		Exchange::Bitfinex,
		Exchange::Bitstamp,
		Exchange::Bybit,
		Exchange::CoinbasePro,
		Exchange::CryptoCom,
		Exchange::Gate,
		Exchange::Huobi,
		Exchange::Kraken,
		Exchange::KuCoin,
		Exchange::Mexc,
		Exchange::Okx,
	];

	pub fn name(self) -> &'static str {
		match self {
			Exchange::Binance => "binance",
			Exchange::Bitfinex => "bitfinex",
			Exchange::Bitstamp => "bitstamp",
			Exchange::Bybit => "bybit",
			Exchange::CoinbasePro => "coinbase_pro",
			Exchange::CryptoCom => "crypto_com",
			Exchange::Gate => "gate",
			Exchange::Huobi => "huobi",
			Exchange::Kraken => "kraken",
			Exchange::KuCoin => "kucoin",
			Exchange::Mexc => "mexc",
			Exchange::Okx => "okx",
		}
	}
}

impl std::fmt::Display for Exchange {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.name())
	}
}

/// One venue's price for one market, in millionths (1e-6) of a quote unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
	pub exchange: Exchange,
	pub price_millionths: u64,
	/// Unix millis at which the venue was read.
	pub at_ms: u64,
}

pub fn now_ms() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Median of a non-empty slice. Writes nothing back permanently — sorts a copy.
fn median(values: &[u64]) -> Option<u64> {
	if values.is_empty() {
		return None
	}
	let mut sorted = values.to_vec();
	sorted.sort_unstable();
	let mid = sorted.len() / 2;
	if sorted.len() % 2 == 0 {
		// Average of the two middles. `u64` addition can overflow only above 2^63 each;
		// prices are nowhere near that, but saturate rather than wrap in a daemon.
		Some(sorted[mid - 1].saturating_add(sorted[mid]) / 2)
	} else {
		Some(sorted[mid])
	}
}

/// Index price for one market: the median across venues that answered *freshly*, and only
/// when at least `min_exchanges` of them did.
///
/// Returning `None` is the intended failure mode: it means "we do not know the price",
/// which is a very different thing from "the price is X" and must never be conflated.
pub fn index_price(
	quotes: &[Quote],
	min_exchanges: usize,
	now_ms: u64,
	max_age_ms: u64,
) -> Option<u64> {
	let fresh: Vec<u64> = quotes
		.iter()
		.filter(|q| now_ms.saturating_sub(q.at_ms) <= max_age_ms)
		.map(|q| q.price_millionths)
		.collect();

	if fresh.len() < min_exchanges {
		return None
	}
	median(&fresh)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn q(exchange: Exchange, price: u64, at_ms: u64) -> Quote {
		Quote { exchange, price_millionths: price, at_ms }
	}

	/// dYdX medianises across venues precisely so one bad venue cannot move the index.
	#[test]
	fn one_wild_venue_does_not_move_the_index() {
		let quotes = [
			q(Exchange::Binance, 60_000_000, 0),
			q(Exchange::Kraken, 60_100_000, 0),
			q(Exchange::Okx, 60_200_000, 0),
			q(Exchange::Gate, 600_000_000, 0),
		];
		assert_eq!(index_price(&quotes, 3, 0, 1_000), Some(60_150_000));
	}

	/// Below quorum we produce nothing at all, not a price built from too few venues.
	#[test]
	fn below_min_exchanges_there_is_no_price() {
		let quotes = [q(Exchange::Binance, 60_000_000, 0), q(Exchange::Kraken, 60_100_000, 0)];
		assert_eq!(index_price(&quotes, 3, 0, 1_000), None);
	}

	/// A venue that stopped answering must not keep contributing.
	#[test]
	fn stale_quotes_are_dropped() {
		let quotes = [
			q(Exchange::Binance, 60_000_000, 0),
			q(Exchange::Kraken, 60_100_000, 0),
			q(Exchange::Okx, 60_200_000, 0),
			q(Exchange::Gate, 999_000_000, 0),
		];
		// Gate is the stale one here: at t=5_000 only it has aged out.
		let mut aged = quotes;
		aged[0].at_ms = 4_900;
		aged[1].at_ms = 4_900;
		aged[2].at_ms = 4_900;
		assert_eq!(index_price(&aged, 3, 5_000, 1_000), Some(60_100_000));
	}

	#[test]
	fn median_of_an_even_count_averages_the_two_middles() {
		assert_eq!(median(&[4, 1, 3, 2]), Some(2));
	}

	#[test]
	fn median_of_nothing_is_nothing() {
		assert_eq!(median(&[]), None);
	}

	/// One venue per response *shape*. These are offline fixtures: the point is that each
	/// envelope is actually read, not that we can reach the network.
	#[test]
	fn every_response_shape_is_parsed() {
		// bare array of objects
		assert_eq!(
			Exchange::Binance
				.parse_ticker(
					"BTCUSDT",
					r#"[{"symbol":"ETHUSDT","bidPrice":"1","askPrice":"2","lastPrice":"3"},
					    {"symbol":"BTCUSDT","bidPrice":"60000","askPrice":"60010","lastPrice":"60005"}]"#
				)
				.unwrap(),
			Ticker { bid_millionths: 60_000_000_000, ask_millionths: 60_010_000_000, last_millionths: 60_005_000_000 }
		);

		// {"data": [...]}
		assert_eq!(
			Exchange::Okx
				.parse_ticker(
					"BTC-USDT",
					r#"{"code":"0","data":[{"instId":"BTC-USDT","bidPx":"60000","askPx":"60010","last":"60005"}]}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// {"result":{"list":[...]}} and {"result":{"data":[...]}}
		assert_eq!(
			Exchange::Bybit
				.parse_ticker(
					"BTCUSDT",
					r#"{"result":{"list":[{"symbol":"BTCUSDT","bid1Price":"60000","ask1Price":"60010","lastPrice":"60005"}]}}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);
		assert_eq!(
			Exchange::CryptoCom
				.parse_ticker(
					"BTC_USDT",
					r#"{"result":{"data":[{"i":"BTC_USDT","b":"60000","k":"60010","a":"60005"}]}}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// {"data":{"ticker":[...]}}
		assert_eq!(
			Exchange::KuCoin
				.parse_ticker(
					"BTC-USDT",
					r#"{"data":{"ticker":[{"symbol":"BTC-USDT","buy":"60000","sell":"60010","last":"60005"}]}}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// single object, fetched per market
		assert_eq!(
			Exchange::CoinbasePro
				.parse_ticker("BTC-USD", r#"{"bid":"60000","ask":"60010","price":"60005"}"#)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);
		assert_eq!(
			Exchange::Bitstamp
				.parse_ticker("btcusd", r#"{"bid":"60000","ask":"60010","last":"60005"}"#)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// array of arrays, with numbers rather than strings
		assert_eq!(
			Exchange::Bitfinex
				.parse_ticker(
					"tBTCUSD",
					r#"[["tETHUSD",1,1,2,2,0,0,3,1,1,1],
					    ["tBTCUSD",60000,1,60010,1,0,0,60005,1,1,1]]"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// {"result":{"<pair>":{"a":[..],"b":[..],"c":[..]}}}
		assert_eq!(
			Exchange::Kraken
				.parse_ticker(
					"XXBTZUSD",
					r#"{"result":{"XXBTZUSD":{"a":["60010","1","1"],"b":["60000","1","1"],"c":["60005","1"]}}}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// Huobi sends numbers rather than strings
		assert_eq!(
			Exchange::Huobi
				.parse_ticker("btcusdt", r#"{"data":[{"symbol":"btcusdt","bid":60000.0,"ask":60010.0,"close":60005.0}]}"#)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);

		// Gate is the remaining array shape; Mexc v3 answers with a single object
		assert_eq!(
			Exchange::Gate
				.parse_ticker(
					"BTC_USDT",
					r#"[{"currency_pair":"BTC_USDT","highest_bid":"60000","lowest_ask":"60010","last":"60005"}]"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);
		assert_eq!(
			Exchange::Mexc
				.parse_ticker(
					"BTCUSDT",
					r#"{"symbol":"BTCUSDT","bidPrice":"60000","askPrice":"60010","lastPrice":"60005"}"#
				)
				.unwrap()
				.price_millionths(),
			60_005_000_000
		);
	}

	/// One response usually holds many markets; a round reads them all at once rather than
	/// asking again per market.
	#[test]
	fn one_response_yields_every_market_in_it() {
		let body = r#"[{"symbol":"ETHUSDT","bidPrice":"1","askPrice":"3","lastPrice":"2"},
		               {"symbol":"BTCUSDT","bidPrice":"60000","askPrice":"60010","lastPrice":"60005"}]"#;
		let all = Exchange::Binance.parse_tickers(body).unwrap();
		assert_eq!(all.len(), 2);
		assert_eq!(all[0].0, "ETHUSDT");
		assert_eq!(all[1].1.price_millionths(), 60_005_000_000);
	}

	/// A market we did not ask for must not come back, and one malformed row must not cost
	/// us the others.
	#[test]
	fn parse_tickers_skips_what_it_cannot_read() {
		let body = r#"[{"symbol":"BTCUSDT","bidPrice":"60000","askPrice":"60010","lastPrice":"60005"},
		               {"symbol":"BROKEN","bidPrice":"x","askPrice":"y","lastPrice":"z"}]"#;
		let all = Exchange::Binance.parse_tickers(body).unwrap();
		assert_eq!(all.len(), 1);
		assert_eq!(all[0].0, "BTCUSDT");
	}

	/// The venues we actually poll must be able to answer several markets at once, or answer
	/// everything in one go — otherwise a round costs one request per market.
	#[test]
	fn the_venues_we_poll_answer_more_than_one_market_at_a_time() {
		for venue in [Exchange::Binance, Exchange::Okx, Exchange::Kraken, Exchange::Bitfinex] {
			match venue.fetch_mode() {
				FetchMode::All => {},
				FetchMode::Batch { max } => assert!(max > 1, "{venue} batches nothing"),
				FetchMode::Single => panic!("{venue} costs one request per market"),
			}
		}
	}

	#[test]
	fn batch_urls_are_built_the_way_each_venue_expects() {
		let symbols = ["BTCUSDT".to_string(), "ETHUSDT".to_string()];
		assert_eq!(
			Exchange::Binance.batch_ticker_url(&symbols).unwrap(),
			"https://data-api.binance.vision/api/v3/ticker/24hr?symbols=\
			 %5B%22BTCUSDT%22%2C%22ETHUSDT%22%5D"
		);
		assert_eq!(
			Exchange::Kraken.batch_ticker_url(&symbols).unwrap(),
			"https://api.kraken.com/0/public/Ticker?pair=BTCUSDT,ETHUSDT"
		);
		// Bitfinex's USDT pairs are written `tDOGE:UST`, so the colon has to be encoded.
		assert_eq!(
			Exchange::Bitfinex
				.batch_ticker_url(&["tBTCUST".to_string(), "tDOGE:UST".to_string()])
				.unwrap(),
			"https://api-pub.bitfinex.com/v2/tickers?symbols=tBTCUST,tDOGE%3AUST"
		);
		// OKX has no batch endpoint; the caller has to take the whole list instead.
		assert!(Exchange::Okx.batch_ticker_url(&symbols).is_none());
	}

	/// Kraken answers its older pairs under the canonical name: ask `XBTUSD`, get
	/// `XXBTZUSD`. Missing that loses the venue with nothing to notice it by.
	#[test]
	fn kraken_answers_under_the_canonical_name() {
		assert!(symbol_matches(Exchange::Kraken, "XXBTZUSD", "XBTUSD"));
		assert!(symbol_matches(Exchange::Kraken, "XBTUSDT", "XBTUSDT"));
		assert!(!symbol_matches(Exchange::Kraken, "XBTUSDT", "XBTUSD"));
	}

	/// The venue list is the reference for what backs our index price; guard its size so
	/// adding a venue is a deliberate act.
	#[test]
	fn dydx_supports_twelve_venues() {
		assert_eq!(Exchange::ALL.len(), 12);
	}

	/// dYdX resolves a venue's price as the median of (ask, bid, last), so one thin print
	/// at the top of the book cannot move the index.
	#[test]
	fn a_venue_price_is_the_median_of_bid_ask_and_last() {
		let t = Ticker { bid_millionths: 100, ask_millionths: 300, last_millionths: 200 };
		assert_eq!(t.price_millionths(), 200);
		// The outlier is dropped whichever end it is on.
		let t = Ticker { bid_millionths: 100, ask_millionths: 200, last_millionths: 9_000 };
		assert_eq!(t.price_millionths(), 200);
	}

	#[test]
	fn parses_binance_shape() {
		let body = r#"[{"symbol":"ETHUSDT","askPrice":"1.00","bidPrice":"2.00","lastPrice":"3.00"},
		               {"symbol":"BTCUSDT","askPrice":"60100.10","bidPrice":"60099.90","lastPrice":"60100.00"}]"#;
		let t = Exchange::Binance.parse_ticker("BTCUSDT", body).unwrap();
		assert_eq!(t.price_millionths(), 60_100_000_000);
	}

	#[test]
	fn parses_okx_shape_and_rejects_an_error_code() {
		let body = r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","askPx":"60100.1","bidPx":"60099.9","last":"60100"}]}"#;
		let t = Exchange::Okx.parse_ticker("BTC-USDT", body).unwrap();
		assert_eq!(t.price_millionths(), 60_100_000_000);
		// OKX reports failures in-band; a non-"0" code must not be read as a price.
		assert!(Exchange::Okx.parse_ticker("BTC-USDT", r#"{"code":"1","data":[]}"#).is_err());
	}

	#[test]
	fn parses_coinbase_shape() {
		let body = r#"{"ask":"60100.10","bid":"60099.90","price":"60100.00"}"#;
		let t = Exchange::CoinbasePro.parse_ticker("BTC-USD", body).unwrap();
		assert_eq!(t.price_millionths(), 60_100_000_000);
	}

	/// A missing listing is a per-venue failure, so the other venues can still reach quorum.
	#[test]
	fn a_missing_listing_is_a_per_venue_failure() {
		assert!(Exchange::Binance.parse_ticker("NOSUCH", r#"[{"symbol":"BTCUSDT"}]"#).is_err());
	}

	#[test]
	fn prices_are_parsed_in_integers_not_floats() {
		assert_eq!(parse_price_millionths("60123.45").unwrap(), 60_123_450_000);
		assert_eq!(parse_price_millionths("1").unwrap(), 1_000_000);
		assert_eq!(parse_price_millionths(" 2.5 ").unwrap(), 2_500_000);
		assert!(parse_price_millionths("-1").is_err());
		assert!(parse_price_millionths("abc").is_err());
	}

	/// Every venue in the dYdX list has an endpoint. Guards against adding one to the enum
	/// and forgetting the adapter, which would silently leave it out of the median.
	#[test]
	fn every_venue_has_an_endpoint() {
		for venue in Exchange::ALL {
			assert!(
				venue.ticker_url("BTCUSDT").is_some(),
				"{venue} is in the list but has no endpoint"
			);
		}
		assert_eq!(Exchange::ALL.len(), 12);
	}
}

// --------------------------------------------------------------------------------------
// Venue adapters
// --------------------------------------------------------------------------------------

/// A venue answered, but not with a usable price.
#[derive(Debug, thiserror::Error)]
pub enum PriceFeedError {
	/// The request itself failed (DNS, TLS, timeout, non-2xx).
	#[error("request to {exchange} failed: {reason}")]
	Request { exchange: Exchange, reason: String },
	/// The venue answered but this market was missing or nonsensical. Not fatal to the
	/// other venues: the caller just counts it out of the quorum.
	#[error("{exchange} has no usable price for {symbol}: {reason}")]
	Unavailable { exchange: Exchange, symbol: String, reason: String },
}

/// What a venue's 24h ticker gives us.
///
/// dYdX resolves a venue's price as the **median of (ask, bid, last)**
/// (`GetMedianPricesFromTickers` in `util.go`), never the last trade on its own — one thin
/// print at the top of the book must not move the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticker {
	pub bid_millionths: u64,
	pub ask_millionths: u64,
	pub last_millionths: u64,
}

impl Ticker {
	pub fn price_millionths(self) -> u64 {
		let mut v = [self.bid_millionths, self.ask_millionths, self.last_millionths];
		v.sort_unstable();
		v[1]
	}
}

/// Parses a decimal price string ("60123.45") into millionths (60_123_450_000).
///
/// Venues send prices as JSON *strings*. Going through `f64` would silently lose precision
/// at exactly the magnitudes we care about, so we do it in integers.
/// Parses a decimal price string ("60123.45") into millionths (60_123_450_000).
///
/// Venues send prices as JSON *strings*. Going through `f64` would silently lose precision
/// at exactly the magnitudes we care about, so we stay in integers.
///
/// Returns the reason as a `String`; the caller knows which venue and market it came from.
pub fn parse_price_millionths(raw: &str) -> Result<u64, String> {
	let raw = raw.trim();
	let signed: i8 = if raw.starts_with('-') { -1 } else { 1 };
	let unsigned = raw.trim_start_matches('-');
	if signed < 0 {
		return Err(format!("negative price {raw}"))
	}
	let (whole, frac) = match unsigned.split_once('.') {
		Some((w, f)) => (w, f),
		None => (unsigned, ""),
	};
	if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
		return Err(format!("not a number: {raw}"))
	}
	let whole: u64 = whole.parse().map_err(|_| format!("not a number: {raw}"))?;
	let mut frac6: String =
		frac.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
	while frac6.len() < 6 {
		frac6.push('0');
	}
	let frac: u64 = frac6.parse().map_err(|_| format!("bad fraction: {raw}"))?;
	Ok(whole.saturating_mul(1_000_000).saturating_add(frac))
}

/// Ticker for a venue that reports one market per request (Coinbase).
fn parse_single_market_ticker(
	exchange: Exchange,
	symbol: &str,
	body: &str,
	fields: (&str, &str, &str),
) -> Result<Ticker, PriceFeedError> {
	let bad = |reason: String| PriceFeedError::Unavailable {
		exchange,
		symbol: symbol.to_string(),
		reason,
	};
	let value: serde_json::Value =
		serde_json::from_str(body).map_err(|e| bad(format!("unparseable json: {e}")))?;
	let (ask_k, bid_k, last_k) = fields;
	let pick = |key: &str| -> Result<u64, PriceFeedError> {
		let raw = value[key].as_str().ok_or_else(|| bad(format!("missing field {key}")))?;
		parse_price_millionths(raw).map_err(|e| bad(format!("{key}: {e}")))
	};
	Ok(Ticker {
		bid_millionths: pick(bid_k)?,
		ask_millionths: pick(ask_k)?,
		last_millionths: pick(last_k)?,
	})
}

/// dYdX fetches *every* market from these venues once per round and filters locally
/// (`IsMultiMarket`), which is far cheaper than one request per market.
fn parse_multi_market(
	exchange: Exchange,
	symbol: &str,
	body: &str,
	pair_key: &str,
	fields: (&str, &str, &str),
) -> Result<Ticker, PriceFeedError> {
	let bad = |reason: String| PriceFeedError::Unavailable {
		exchange,
		symbol: symbol.to_string(),
		reason,
	};
	let value: serde_json::Value =
		serde_json::from_str(body).map_err(|e| bad(format!("unparseable json: {e}")))?;
	let list = value
		.as_array()
		.or_else(|| value.get("data").and_then(|d| d.as_array()))
		.ok_or_else(|| bad("response is not a list of tickers".to_string()))?;
	let (ask_k, bid_k, last_k) = fields;
	let entry = list
		.iter()
		.find(|e| e[pair_key].as_str() == Some(symbol))
		.ok_or_else(|| bad(format!("no listing for {symbol}")))?;
	let pick = |entry: &serde_json::Value, key: &str| -> Result<u64, PriceFeedError> {
		let s = entry[key].as_str().ok_or_else(|| bad(format!("missing field {key}")))?;
		parse_price_millionths(s).map_err(|_| bad(format!("bad field {key}")))
	};
	Ok(Ticker {
		bid_millionths: pick(entry, bid_k)?,
		ask_millionths: pick(entry, ask_k)?,
		last_millionths: pick(entry, last_k)?,
	})
}

impl Exchange {
	/// How this venue can be asked for several markets at once. See [`FetchMode`].
	pub fn fetch_mode(self) -> FetchMode {
		match self {
			// `?symbols=["…","…"]`. The gateway answers 414 somewhere past 450 symbols
			// (8.5 kB of query), so we keep well below that.
			Exchange::Binance => FetchMode::Batch { max: 400 },
			// `?symbols=tBTCUSD,tETHUSD`. Verified with 100 symbols.
			Exchange::Bitfinex => FetchMode::Batch { max: 100 },
			// `?pair=XBTUSDT,ETHUSDT`. Verified with 7; documented as "a few dozen", so we
			// stay small — and a batch that fails is retried market by market anyway.
			Exchange::Kraken => FetchMode::Batch { max: 20 },
			// One market per request; no batch parameter exists.
			Exchange::Mexc | Exchange::CoinbasePro | Exchange::Bitstamp => FetchMode::Single,
			// The rest return every market in one response and have no filter that works.
			Exchange::Okx
			| Exchange::Bybit
			| Exchange::CryptoCom
			| Exchange::Gate
			| Exchange::Huobi
			| Exchange::KuCoin => FetchMode::All,
		}
	}

	/// REST endpoint for this venue's 24h ticker for **one** market, or `None` for a venue
	/// we have not wired yet (all twelve are wired now). Endpoints follow dYdX v4's
	/// `exchange_query_details.go`.
	///
	/// Venues that can take several markets at once have [`Exchange::batch_ticker_url`];
	/// this one is the fallback used when a batch has to be retried one market at a time.
	pub fn ticker_url(self, symbol: &str) -> Option<String> {
		Some(match self {
			// dYdX: `/api/v3/ticker/24hr` on data-api.binance.vision. Without `symbol` this
			// returns all 3 739 pairs (~1.9 MB), which is exactly what we must not do per
			// market; see `batch_ticker_url` for the multi-market form.
			Exchange::Binance =>
				format!("https://data-api.binance.vision/api/v3/ticker/24hr?symbol={}", pct(symbol)),
			// dYdX: `/api/v5/market/tickers?instType=SPOT`.
			Exchange::Okx => "https://www.okx.com/api/v5/market/tickers?instType=SPOT".to_string(),
			// dYdX hits one product at a time (`/products/$/ticker`), and we deviate here.
			// v4 still uses `api.pro.coinbase.com`, which now answers **503** — Coinbase
			// retired the Coinbase Pro hostname when it folded Pro into Advanced Trade.
			// Verified 2026-09-17: `api.exchange.coinbase.com` answers 200 with the same
			// schema (`ask`, `bid`, `price`), so only the hostname changes.
			Exchange::CoinbasePro =>
				format!("https://api.exchange.coinbase.com/products/{symbol}/ticker"),
			Exchange::Bitstamp => format!("https://www.bitstamp.net/api/v2/ticker/{}", pct(symbol)),
			// dYdX asks for `symbols=ALL` and filters locally. We ask for ours by name:
			// one market is ~120 B against ~1 MB for the whole list.
			Exchange::Bitfinex =>
				format!("https://api-pub.bitfinex.com/v2/tickers?symbols={}", pct(symbol)),
			Exchange::Bybit => "https://api.bybit.com/v5/market/tickers?category=spot".to_string(),
			Exchange::CryptoCom => "https://api.crypto.com/v2/public/get-ticker".to_string(),
			Exchange::Gate => "https://api.gateio.ws/api/v4/spot/tickers".to_string(),
			Exchange::Huobi => "https://api.huobi.pro/market/tickers".to_string(),
			// Without `pair` this returns every market; one pair is ~300 B.
			Exchange::Kraken => format!("https://api.kraken.com/0/public/Ticker?pair={}", pct(symbol)),
			Exchange::KuCoin => "https://api.kucoin.com/api/v1/market/allTickers".to_string(),
			// v4 still points at MEXC's retired v2 endpoint, which now answers 404. v3 works and
			// uses the same field names as Binance.
			Exchange::Mexc =>
				format!("https://api.mexc.com/api/v3/ticker/24hr?symbol={}", pct(symbol)),
			})
			}

			/// REST endpoint for **several** markets at once, or `None` for a venue that cannot do
			/// that. This is what keeps a round at a handful of requests instead of one per market.
			pub fn batch_ticker_url(self, symbols: &[String]) -> Option<String> {
			if symbols.is_empty() {
				return None
			}
			Some(match self {
				// `?symbols=["BTCUSDT","ETHUSDT"]`, percent-encoded.
				Exchange::Binance => {
					let list: Vec<String> = symbols.iter().map(|s| format!("\"{s}\"")).collect();
					format!(
						"https://data-api.binance.vision/api/v3/ticker/24hr?symbols={}",
						pct(&format!("[{}]", list.join(",")))
					)
				},
				Exchange::Bitfinex => format!(
					"https://api-pub.bitfinex.com/v2/tickers?symbols={}",
					join_pct(symbols, ",")
				),
				Exchange::Kraken => format!(
					"https://api.kraken.com/0/public/Ticker?pair={}",
					join_pct(symbols, ",")
				),
				// No batch parameter exists: the caller falls back to one request per market.
				_ => return None,
			})
			}


	/// Turns a venue's response body into a [`Ticker`], read on that venue's own terms.
	///
	/// Every venue sends a different envelope — some a bare array, some `{"data": …}`, and two
	/// (Bitfinex and Kraken) not objects at all. Pretending they share a shape is how parsers
	/// quietly return nothing, so each is handled separately.
	/// One request usually carries many markets — a round is a handful of requests, not one
	/// per market — so this returns every ticker the body holds, keyed by the venue's own
	/// spelling. An entry that cannot be read is skipped rather than fatal: Kraken answers a
	/// batch with the pairs it recognised, and one malformed row must not cost us the rest.
	pub fn parse_tickers(self, body: &str) -> Result<Vec<(String, Ticker)>, PriceFeedError> {
		let bad = |reason: String| PriceFeedError::Unavailable {
			exchange: self,
			symbol: String::new(),
			reason,
		};
		let value: serde_json::Value =
			serde_json::from_str(body).map_err(|e| bad(format!("unparseable json: {e}")))?;

		// Bitfinex answers with an array of arrays: [SYMBOL, BID, BID_SIZE, ASK, ASK_SIZE,
		// CHANGE, CHANGE_RELATIVE, LAST_PRICE, VOLUME, HIGH, LOW].
		if self == Exchange::Bitfinex {
			let rows =
				value.as_array().ok_or_else(|| bad("response is not a list of tickers".into()))?;
			let mut out = Vec::new();
			for row in rows {
				let Some(symbol) = row.get(0).and_then(|s| s.as_str()) else { continue };
				// Bitfinex sends JSON numbers, not decimal strings.
				let at = |i: usize| -> Option<u64> {
					let raw = match row.get(i)? {
						serde_json::Value::String(v) => v.clone(),
						serde_json::Value::Number(n) => format!("{:.8}", n.as_f64()?),
						_ => return None,
					};
					parse_price_millionths(&raw).ok()
				};
				if let (Some(bid), Some(ask), Some(last)) = (at(1), at(3), at(7)) {
					out.push((
						symbol.to_string(),
						Ticker {
							bid_millionths: bid,
							ask_millionths: ask,
							last_millionths: last,
						},
					));
				}
			}
			return Ok(out)
		}

		// Kraken answers with `{"result": {"<pair>": {"a": [...], "b": [...], "c": [...]}}}`,
		// where each of `a`/`b`/`c` is [price, …] and `c` is the last trade.
		if self == Exchange::Kraken {
			let result = value["result"]
				.as_object()
				.ok_or_else(|| bad("response has no `result`".into()))?;
			let mut out = Vec::new();
			for (symbol, ticker) in result {
				let first = |key: &str| -> Option<u64> {
					let raw = ticker.get(key)?.get(0)?.as_str()?;
					parse_price_millionths(raw).ok()
				};
				if let (Some(bid), Some(ask), Some(last)) = (first("b"), first("a"), first("c")) {
					out.push((
						symbol.clone(),
						Ticker {
							bid_millionths: bid,
							ask_millionths: ask,
							last_millionths: last,
						},
					));
				}
			}
			return Ok(out)
		}

		let entries: Vec<serde_json::Value> = match self {
			Exchange::Binance | Exchange::Gate | Exchange::Mexc => value
				.as_array()
				.cloned()
				.unwrap_or_else(|| vec![value.clone()]),
			Exchange::Okx | Exchange::Huobi => value["data"]
				.as_array()
				.ok_or_else(|| bad("response has no `data` list".into()))?
				.clone(),
			Exchange::Bybit => value["result"]["list"]
				.as_array()
				.ok_or_else(|| bad("response has no `result.list`".into()))?
				.clone(),
			Exchange::CryptoCom => value["result"]["data"]
				.as_array()
				.ok_or_else(|| bad("response has no `result.data`".into()))?
				.clone(),
			Exchange::KuCoin => value["data"]["ticker"]
				.as_array()
				.ok_or_else(|| bad("response has no `data.ticker`".into()))?
				.clone(),
			// Both are fetched per market, so the body is the ticker itself.
			Exchange::CoinbasePro | Exchange::Bitstamp => vec![value.clone()],
			Exchange::Bitfinex | Exchange::Kraken => unreachable!("handled above"),
		};

		let (pair_key, bid_key, ask_key, last_key) = match self {
			Exchange::Binance => ("symbol", "bidPrice", "askPrice", "lastPrice"),
			Exchange::Okx => ("instId", "bidPx", "askPx", "last"),
			Exchange::Bybit => ("symbol", "bid1Price", "ask1Price", "lastPrice"),
			Exchange::CryptoCom => ("i", "b", "k", "a"),
			Exchange::Gate => ("currency_pair", "highest_bid", "lowest_ask", "last"),
			Exchange::Huobi => ("symbol", "bid", "ask", "close"),
			Exchange::KuCoin => ("symbol", "buy", "sell", "last"),
			Exchange::Mexc => ("symbol", "bidPrice", "askPrice", "lastPrice"),
			Exchange::CoinbasePro => ("", "bid", "ask", "price"),
			Exchange::Bitstamp => ("", "bid", "ask", "last"),
			Exchange::Bitfinex | Exchange::Kraken => unreachable!("handled above"),
		};

		let mut out = Vec::new();
		for entry in &entries {
			let symbol = if pair_key.is_empty() {
				// fetched per market, so the body carries no symbol of its own
				String::new()
			} else {
				match entry[pair_key].as_str() {
					Some(s) => s.to_string(),
					None => continue,
				}
			};

			// Huobi sends numbers; everyone else sends decimal strings.
			let read = |key: &str| -> Option<u64> {
				let raw = match entry.get(key)? {
					serde_json::Value::String(s) => s.clone(),
					serde_json::Value::Number(n) => format!("{:.8}", n.as_f64()?),
					_ => return None,
				};
				parse_price_millionths(&raw).ok()
			};

			if let (Some(bid), Some(ask), Some(last)) =
				(read(bid_key), read(ask_key), read(last_key))
			{
				out.push((
					symbol,
					Ticker {
						bid_millionths: bid,
						ask_millionths: ask,
						last_millionths: last,
					},
				));
			}
		}
		Ok(out)
	}

	/// The [`Ticker`] for one market out of a venue's response. See
	/// [`Exchange::parse_tickers`], which is what a round actually uses.
	pub fn parse_ticker(self, symbol: &str, body: &str) -> Result<Ticker, PriceFeedError> {
		let all = self.parse_tickers(body)?;
		if let Some((_, ticker)) = all.iter().find(|(s, _)| symbol_matches(self, s, symbol)) {
			return Ok(*ticker)
		}
		// Fetched per market (Coinbase, Bitstamp): the body *is* the ticker and carries no
		// symbol to match on, so there is exactly one entry and it has no name.
		if let Some((_, ticker)) = all.iter().find(|(s, _)| s.is_empty()) {
			return Ok(*ticker)
		}
		Err(PriceFeedError::Unavailable {
			exchange: self,
			symbol: symbol.to_string(),
			reason: format!("no listing for {symbol}"),
		})
	}
}

/// Fetches one venue's price. A failure here is per-venue: the caller still needs the
/// other venues to reach quorum.
pub async fn fetch_quote(
	client: &reqwest::Client,
	exchange: Exchange,
	symbol: &str,
) -> Result<Quote, PriceFeedError> {
	let url = exchange.ticker_url(symbol).ok_or_else(|| PriceFeedError::Unavailable {
		exchange,
		symbol: symbol.to_string(),
		reason: "no adapter for this venue yet".to_string(),
	})?;
	let response = client
		.get(&url)
		.send()
		.await
		.map_err(|e| PriceFeedError::Request { exchange, reason: e.to_string() })?
		.error_for_status()
		.map_err(|e| PriceFeedError::Request { exchange, reason: e.to_string() })?;
	let body = response
		.text()
		.await
		.map_err(|e| PriceFeedError::Request { exchange, reason: e.to_string() })?;
	let ticker = exchange.parse_ticker(symbol, &body)?;
	Ok(Quote { exchange, price_millionths: ticker.price_millionths(), at_ms: now_ms() })
}

/// Percent-encodes just what a query cannot carry literally.
///
/// Symbols are alphanumeric apart from Bitfinex's `tDOGE:UST`; Binance's batch form also
/// needs `[`, `]`, `"` and `,` encoded. Anything else would already be a broken symbol.
fn pct(symbol: &str) -> String {
	symbol
		.replace('%', "%25")
		.replace('[', "%5B")
		.replace(']', "%5D")
		.replace('"', "%22")
		.replace(',', "%2C")
		.replace(':', "%3A")
}

/// Several symbols joined by a separator that must survive encoding (`,` for Bitfinex and
/// Kraken), so only the symbols themselves are encoded.
fn join_pct(symbols: &[String], separator: &str) -> String {
	symbols.iter().map(|s| pct(s)).collect::<Vec<_>>().join(separator)
}

/// Whether a symbol a venue answered with is the one we asked for.
///
/// Kraken answers its older pairs under their canonical name: ask for `XBTUSD` and the
/// result is keyed `XXBTZUSD`. Its USDT pairs come back as asked, but the alias has to be
/// understood either way, and the consequence of missing it is a silently absent venue.
pub fn symbol_matches(exchange: Exchange, got: &str, want: &str) -> bool {
	if got == want {
		return true
	}
	exchange == Exchange::Kraken && kraken_alias(got) == kraken_alias(want)
}

/// `XXBTZUSD` and `XBTUSD` are the same pair; `XBTUSDT` is already canonical.
fn kraken_alias(symbol: &str) -> String {
	let without_prefix = symbol.strip_prefix('X').unwrap_or(symbol).to_string();
	let without_quote_prefix = without_prefix
		.replace("ZUSD", "USD")
		.replace("ZUSDT", "USDT")
		.replace("ZEUR", "EUR");
	without_quote_prefix.strip_prefix('X').unwrap_or(&without_quote_prefix).to_string()
}

async fn get_body(client: &reqwest::Client, url: &str) -> Result<String, String> {
	client
		.get(url)
		.send()
		.await
		.map_err(|e| e.to_string())?
		.error_for_status()
		.map_err(|e| e.to_string())?
		.text()
		.await
		.map_err(|e| e.to_string())
}

/// Fetches every asked-for market from one venue, in as few requests as the venue allows.
///
/// This is the difference between a feed that costs a handful of requests a round and one
/// that costs one request *per market per round* — which, with a venue that answers with
/// its whole book, is how four markets turned into 10.6 MB every thirty seconds.
///
/// Returns only the markets that were asked for, each under the venue's own spelling; a
/// venue that could not be reached simply contributes nothing, which is the same per-venue
/// failure the median already expects.
pub async fn fetch_many(
	client: &reqwest::Client,
	exchange: Exchange,
	symbols: &[String],
) -> Vec<(String, Quote)> {
	let mut out: Vec<(String, Quote)> = Vec::new();
	if symbols.is_empty() {
		return out
	}

	let take = |out: &mut Vec<(String, Quote)>, all: &[(String, Ticker)], asked: &[String]| {
		let at_ms = now_ms();
		for (symbol, ticker) in all {
			if asked.iter().any(|s| symbol_matches(exchange, symbol, s)) {
				out.push((
					symbol.clone(),
					Quote {
						exchange,
						price_millionths: ticker.price_millionths(),
						at_ms,
					},
				));
			}
		}
	};

	match exchange.fetch_mode() {
		// No batch endpoint: one request returns every market and we pick ours out of it.
		FetchMode::All => {
			let Some(url) = exchange.ticker_url("") else { return out };
			let Ok(body) = get_body(client, &url).await else { return out };
			if let Ok(all) = exchange.parse_tickers(&body) {
				take(&mut out, &all, symbols);
			}
		},
		FetchMode::Batch { max } =>
			for chunk in symbols.chunks(max) {
				let mut answered = false;
				if let Some(url) = exchange.batch_ticker_url(chunk) {
					if let Ok(body) = get_body(client, &url).await {
						if let Ok(all) = exchange.parse_tickers(&body) {
							answered = !all.is_empty();
							take(&mut out, &all, chunk);
						}
					}
				}
				// Kraken answers a batch containing one unknown pair with an empty result
				// rather than the pairs it did recognise, so a batch that came back empty is
				// retried market by market: one bad symbol then costs one market, not the lot.
				if !answered {
					for symbol in chunk {
						if let Ok(quote) = fetch_quote(client, exchange, symbol).await {
							out.push((symbol.clone(), quote));
						}
					}
				}
			},
		FetchMode::Single =>
			for symbol in symbols {
				if let Ok(quote) = fetch_quote(client, exchange, symbol).await {
					out.push((symbol.clone(), quote));
				}
			},
	}

	out
}

// --------------------------------------------------------------------------------------
// Quote-currency conversion
//
// dYdX v4 does this in `price_encoder.convertPriceUpdate`. Venues quote the same market in
// different currencies — BTC-USDT and BTC-USD are *not* the same number — so each venue's
// raw price has to be brought onto one currency before it can be medianised with the
// others.
// --------------------------------------------------------------------------------------

/// A market whose index price is used as the conversion rate (e.g. `USDT-USD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjustByMarket {
	pub market: &'static str,
	/// How many venues must price this market before we trust it as a rate.
	pub min_exchanges: usize,
}

/// How to bring one venue's quote onto our quote currency (USD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConversionDetails {
	/// The venue quotes the pair the other way round (quote per base rather than base per
	/// quote), so we need the reciprocal.
	pub invert: bool,
	/// Multiply (or, with `invert`, divide) by this market's index price.
	pub adjust_by: Option<AdjustByMarket>,
}

impl ConversionDetails {
	/// Already in the right currency, right way round.
	pub const NONE: ConversionDetails = ConversionDetails { invert: false, adjust_by: None };

	pub const fn inverted() -> ConversionDetails {
		ConversionDetails { invert: true, adjust_by: None }
	}

	pub const fn adjust_by(market: &'static str, min_exchanges: usize) -> ConversionDetails {
		ConversionDetails {
			invert: false,
			adjust_by: Some(AdjustByMarket { market, min_exchanges }),
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
	/// We need a conversion rate we could not establish.
	///
	/// This must never fall back to the unconverted price: reporting BTC-in-USDT as though
	/// it were BTC-in-USD is a worse failure than reporting nothing, because it looks fine.
	/// v4 takes the same position and errors the ticker out.
	#[error(
		"no index price for adjust-by market {market}: {answered} venue(s) priced it, {required} required"
	)]
	NoAdjustByRate { market: String, answered: usize, required: usize },
	#[error("adjust-by market {market} is priced at zero")]
	ZeroAdjustByRate { market: String },
}

// Fixed-point helpers. Prices are in millionths (1e-6); the intermediates do not fit in a
// `u64`, so they are computed in `u128`.

/// `(a/1e6) * (b/1e6)`, in millionths.
fn mul_millionths(a: u64, b: u64) -> u64 {
	((a as u128 * b as u128) / 1_000_000) as u64
}

/// `1 / (a/1e6)`, in millionths.
///
/// Exposed because turning a market's quote currency around is a real operation, not an
/// implementation detail: we quote the same ping in USD (usa "USDT-USD") but our markets are
/// **USDT**-quoted, so a USD venue joins the median by multiplying with the reciprocal,
/// "how many USDT a dollar buys" ("USD-USDT").
pub fn invert_millionths(a: u64) -> u64 {
	(1_000_000_000_000u128 / a as u128) as u64
}

/// `(a/1e6) / (b/1e6)`, in millionths.
fn div_millionths(a: u64, b: u64) -> u64 {
	((a as u128 * 1_000_000u128) / b as u128) as u64
}

/// Converts one venue's raw price into our quote currency.
///
/// `adjust_by` carries the rate together with how many venues priced it, because v4 checks
/// the venue count *after* fetching and errors out when it falls short.
pub fn convert(
	raw_millionths: u64,
	details: ConversionDetails,
	adjust_by: Option<(u64, usize)>,
) -> Result<u64, ConversionError> {
	let Some(by) = details.adjust_by else {
		return Ok(if details.invert {
			invert_millionths(raw_millionths)
		} else {
			raw_millionths
		})
	};

	let (rate, answered) = adjust_by
		.ok_or_else(|| ConversionError::NoAdjustByRate {
			market: by.market.to_string(),
			answered: 0,
			required: by.min_exchanges,
		})?;
	if answered < by.min_exchanges {
		return Err(ConversionError::NoAdjustByRate {
			market: by.market.to_string(),
			answered,
			required: by.min_exchanges,
		})
	}
	if rate == 0 {
		return Err(ConversionError::ZeroAdjustByRate { market: by.market.to_string() })
	}

	Ok(if details.invert {
		div_millionths(rate, raw_millionths)
	} else {
		mul_millionths(raw_millionths, rate)
	})
}

#[cfg(test)]
mod conversion_tests {
	use super::*;

	const M: u64 = 1_000_000; // one whole unit, in millionths

	#[test]
	fn no_conversion_leaves_the_price_alone() {
		assert_eq!(convert(42 * M, ConversionDetails::NONE, None).unwrap(), 42 * M);
	}

	#[test]
	fn invert_takes_the_reciprocal() {
		// A venue quoting 0.5 USD-per-unit rather than 2 units-per-USD.
		assert_eq!(convert(M / 2, ConversionDetails::inverted(), None).unwrap(), 2 * M);
	}

	/// The BTC case: a USDT-quoted venue has to be multiplied by USDT's own price.
	#[test]
	fn usdt_quoted_price_is_multiplied_by_the_usdt_usd_rate() {
		assert_eq!(
			convert(100 * M, ConversionDetails::adjust_by("USDT-USD", 2), Some((M + 20_000, 2)))
				.unwrap(),
			102 * M
		);
	}

	#[test]
	fn inverted_and_adjusted_divides_by_the_raw_price() {
		// rate / raw = 1.02 / 4 = 0.255
		assert_eq!(
			convert(
				4 * M,
				ConversionDetails { invert: true, adjust_by: Some(AdjustByMarket { market: "USDT-USD", min_exchanges: 1 }) },
				Some((M + 20_000, 1)),
			).unwrap(),
			255_000
		);
	}

	/// The anti-fallback property, and the reason to have this layer at all: when the rate
	/// is untrustworthy we lose the venue, we do not quietly use its raw price.
	#[test]
	fn a_rate_below_quorum_is_not_silently_skipped() {
		let result =
			convert(100 * M, ConversionDetails::adjust_by("USDT-USD", 2), Some((M, 1)));
		assert!(matches!(
			result,
			Err(ConversionError::NoAdjustByRate { answered: 1, required: 2, .. })
		));
	}

	#[test]
	fn a_missing_rate_altogether_is_an_error() {
		assert!(convert(100 * M, ConversionDetails::adjust_by("USDT-USD", 2), None).is_err());
	}

	/// The direction change: our markets are USDT-quoted, so a USD venue is brought over
	/// with "USDT per USD", which is the reciprocal of the quoted "USDT-USD".
	///
	/// Note this is *not* dYdX's own `Invert`: that inverts the market price. Here we invert
	/// the **rate**, and still multiply. dYdX can use plain multiply only because its markets
	/// are USD-quoted.
	#[test]
	fn a_usd_venue_joins_a_usdt_market_via_the_inverted_rate() {
		// 1 USDT is worth 0.99 USD, so 1 USD buys 1/0.99 USDT.
		let usdt_per_usd = invert_millionths(990_000);
		// A USD venue quoting BTC at 100 USD becomes ~101.0101 USDT.
		let converted = convert(
			100 * M,
			ConversionDetails::adjust_by("USD-USDT", 2),
			Some((usdt_per_usd, 2)),
		)
		.unwrap();
		assert_eq!(converted, 101_010_100);
	}

	/// The same venue's price must survive unchanged when it already quotes our currency.
	#[test]
	fn a_usdt_venue_needs_no_conversion() {
		assert_eq!(
			convert(76908 * M, ConversionDetails::NONE, None).unwrap(),
			76908 * M
		);
	}

	#[test]
	fn a_zero_rate_is_rejected() {
		assert!(matches!(
			convert(100 * M, ConversionDetails::adjust_by("USDT-USD", 1), Some((0, 1))),
			Err(ConversionError::ZeroAdjustByRate { .. })
		));
	}
}
