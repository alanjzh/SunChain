//! Manual price-feed check — run this on a machine that can actually reach the venues.
//!
//! Unit tests can only verify arithmetic, because they have no network. This performs the
//! real requests, so a shape mistake (redirect, rate limit, renamed field) shows up as a
//! visible failure rather than as a silently missing venue.
//!
//! ```text
//! cargo run --bin pricefeed-check
//! ```
//!
//! The point of the exercise is the gap between the two medians at the bottom. Our markets
//! are **USDT**-quoted, so every venue has to be brought onto USDT before it can be
//! medianised with the others; without that we are averaging different currencies.

use sunchain::pricefeed::{self, ConversionDetails, Exchange, Quote};

fn fmt(v: u64) -> String {
	format!("{}.{:06}", v / 1_000_000, v % 1_000_000)
}

fn flag(args: &[String], name: &str, default: &str) -> String {
	args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone()).unwrap_or(default.into())
}

#[tokio::main]
async fn main() {
	let args: Vec<String> = std::env::args().collect();
	if args.iter().any(|a| a == "--help" || a == "-h") {
		println!(
			"usage: pricefeed-check\n\
			 \n\
			   --btc-binance   SYM  (default BTCUSDT)   quoted in USDT\n\
			   --btc-okx       SYM  (default BTC-USDT)  quoted in USDT\n\
			   --btc-coinbase  SYM  (default BTC-USD)   quoted in USD\n\
			   --usd-okx       SYM  (default USDT-USD)  the peg rate source\n\
			   --usd-coinbase  SYM  (default USDT-USD)  the peg rate source\n\
			   --min-exchanges N    (default 2)\n\
			   --usd-min       N    (default 1)"
		);
		return
	}

	let min_exchanges = flag(&args, "--min-exchanges", "2").parse().expect("bad number");
	let usd_min = flag(&args, "--usd-min", "1").parse().expect("bad number");

	// The target market is USDT-quoted, so USDT venues need nothing and the USD venue needs
	// multiplying by how many USDT one dollar buys.
	let usdt_in_usd = "USDT-USD";
	let target: Vec<(Exchange, String, ConversionDetails)> = vec![
		(
			Exchange::Binance,
			flag(&args, "--btc-binance", "BTCUSDT"),
			ConversionDetails::NONE,
		),
		(Exchange::Okx, flag(&args, "--btc-okx", "BTC-USDT"), ConversionDetails::NONE),
		(
			Exchange::CoinbasePro,
			flag(&args, "--btc-coinbase", "BTC-USD"),
			ConversionDetails::adjust_by("USD-USDT", usd_min),
		),
	];

	let rate_sources: Vec<(Exchange, String)> = vec![
		(Exchange::Okx, flag(&args, "--usd-okx", usdt_in_usd)),
		(Exchange::CoinbasePro, flag(&args, "--usd-coinbase", usdt_in_usd)),
	];

	let client = reqwest::Client::builder()
		.user_agent("sunchain-pricefeed-check")
		.timeout(std::time::Duration::from_secs(10))
		.build()
		.expect("client");

	// Phase 1: the peg. `USDT-USD` is quoted as "USD per USDT"; we need its reciprocal.
	let mut rate_quotes: Vec<Quote> = Vec::new();
	let mut rate_failures: Vec<String> = Vec::new();
	for (exchange, symbol) in &rate_sources {
		match pricefeed::fetch_quote(&client, *exchange, symbol).await {
			Ok(q) => rate_quotes.push(q),
			Err(e) => rate_failures.push(format!("{exchange} ({symbol}): {e}")),
		}
	}
	let now = pricefeed::now_ms();
	let maybe_rate = pricefeed::index_price(&rate_quotes, usd_min, now, 30_000);

	println!("peg          {usdt_in_usd}");
	for (exchange, symbol) in &rate_sources {
		match rate_quotes.iter().find(|q| &q.exchange == exchange) {
			Some(q) => println!("  {:<14} {:<10} {}", exchange, symbol, fmt(q.price_millionths)),
			None => println!("  {:<14} {:<10} FAILED", exchange, symbol),
		}
	}
	for f in &rate_failures {
		println!("  ! {f}");
	}
	let usdt_per_usd = maybe_rate.map(pricefeed::invert_millionths);
	match usdt_per_usd {
		Some(r) => println!("  USD-USDT      {}   (reciprocal, {}/{})", fmt(r), rate_quotes.len(), usd_min),
		None => println!("  USD-USDT      NONE — the USD venue will be dropped, not misquoted"),
	}

	// Phase 2: the target market.
	let mut raw_quotes: Vec<Quote> = Vec::new();
	let mut converted: Vec<(Exchange, u64)> = Vec::new();
	let mut failures: Vec<String> = Vec::new();

	for (exchange, symbol, details) in &target {
		let raw = match pricefeed::fetch_quote(&client, *exchange, symbol).await {
			Ok(q) => q,
			Err(e) => {
				failures.push(format!("{exchange} ({symbol}): {e}"));
				continue
			},
		};
		raw_quotes.push(raw);
		match pricefeed::convert(raw.price_millionths, *details, usdt_per_usd.map(|r| (r, rate_quotes.len()))) {
			Ok(p) => converted.push((*exchange, p)),
			Err(e) => failures.push(format!("{exchange} ({symbol}): {e}")),
		}
	}

	// Sweep every venue, so a wired-but-unreachable one is visible rather than inferred.
	println!("\nvenue sweep (BTC, raw)");
	let sweep: Vec<(pricefeed::Exchange, &str)> = vec![
		(pricefeed::Exchange::Binance, "BTCUSDT"),
		(pricefeed::Exchange::Okx, "BTC-USDT"),
		(pricefeed::Exchange::CoinbasePro, "BTC-USD"),
		(pricefeed::Exchange::Bitstamp, "btcusd"),
		(pricefeed::Exchange::Bitfinex, "tBTCUSD"),
		(pricefeed::Exchange::Bybit, "BTCUSDT"),
		(pricefeed::Exchange::CryptoCom, "BTC_USDT"),
		(pricefeed::Exchange::Gate, "BTC_USDT"),
		(pricefeed::Exchange::Huobi, "btcusdt"),
		(pricefeed::Exchange::Kraken, "XXBTZUSD"),
		(pricefeed::Exchange::KuCoin, "BTC-USDT"),
		(pricefeed::Exchange::Mexc, "BTCUSDT"),
	];
	for (venue, symbol) in &sweep {
		match pricefeed::fetch_quote(&client, *venue, symbol).await {
			Ok(q) => println!(
				"  {:<14} {:<10} {}",
				venue,
				symbol,
				q.price_millionths as f64 / 1_000_000.0
			),
			Err(e) => println!("  {:<14} {:<10} FAILED: {}", venue, symbol, e),
		}
	}

	println!("\ntarget market (BTC), quote asset USDT");
	println!("{:<16} {:<11} {:>15} {:>15}", "venue", "symbol", "raw", "in USDT");
	for (exchange, symbol, _) in &target {
		let raw = raw_quotes.iter().find(|q| &q.exchange == exchange);
		let conv = converted.iter().find(|(e, _)| e == exchange);
		println!(
			"{:<16} {:<11} {:>15} {:>15}",
			exchange,
			symbol,
			raw.map(|q| fmt(q.price_millionths)).unwrap_or_else(|| "FAILED".into()),
			conv.map(|(_, p)| fmt(*p)).unwrap_or_else(|| {
				if raw.is_some() { "dropped".into() } else { "-".into() }
			}),
		);
	}
	for f in &failures {
		println!("  ! {f}");
	}

	let Median = |mut v: Vec<u64>, min: usize| -> Option<u64> {
		if v.len() < min {
			return None
		}
		v.sort_unstable();
		Some(v[v.len() / 2])
	};

	let raw_prices: Vec<u64> = raw_quotes.iter().map(|q| q.price_millionths).collect();
	let conv_prices: Vec<u64> = converted.iter().map(|(_, p)| *p).collect();

	println!(
		"\nmedian WITHOUT conversion   {}",
		Median(raw_prices, 1).map(fmt).unwrap_or_else(|| "NONE".into())
	);
	println!(
		"median WITH conversion      {}   (quorum {}/{})",
		Median(conv_prices.clone(), min_exchanges).map(fmt).unwrap_or_else(|| "NONE".into()),
		conv_prices.len(),
		min_exchanges
	);
}
