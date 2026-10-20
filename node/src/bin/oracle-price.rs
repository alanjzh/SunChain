//! Reads the aggregated index price straight off the chain.
//!
//! The feed logging "published a price vote" only proves a vote was *submitted*. This proves
//! the other half: that the quotes were actually aggregated into an index price. It prints
//! the stored `PricePoint` for a market, which is written by `pallet-oracle`'s `on_initialize`
//! and by nothing else.
//!
//! ```text
//! cargo run --bin oracle-price -- http://localhost:9944 1
//! ```

use scale_codec::{Decode, Encode};
use pallet_oracle::PricePoint;

fn storage_key(market_id: u32) -> String {
	// twox128("Oracle") ++ twox128("MarketPrices") ++ blake2_128Concat(MarketId)
	let mut key = sp_core::hashing::twox_128(b"Oracle").to_vec();
	key.extend_from_slice(&sp_core::hashing::twox_128(b"MarketPrices"));
	let encoded = market_id.encode();
	key.extend_from_slice(&sp_core::hashing::blake2_128(&encoded));
	key.extend_from_slice(&encoded);
	format!("0x{}", key.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
	let s = s.strip_prefix("0x").unwrap_or(s);
	if s.len() % 2 != 0 {
		return None
	}
	(0..s.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
		.collect()
}

#[tokio::main]
async fn main() {
	let args: Vec<String> = std::env::args().collect();
	let url = args.get(1).cloned().unwrap_or_else(|| "http://127.0.0.1:9944".into());
	let market_id: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
	let tick_size_quote: u128 =
		args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);

	let key = storage_key(market_id);
	let body = serde_json::json!({
		"jsonrpc": "2.0", "id": 1,
		"method": "state_getStorage",
		"params": [key],
	});

	let response = match reqwest::Client::new().post(&url).json(&body).send().await {
		Ok(r) => r,
		Err(e) => {
			eprintln!("cannot reach {url}: {e}");
			std::process::exit(1)
		},
	};
	let text = response.text().await.unwrap_or_default();
	let value: serde_json::Value = match serde_json::from_str(&text) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("bad rpc response: {e}\n{text}");
			std::process::exit(1)
		},
	};

	match value.get("result") {
		Some(serde_json::Value::Null) | None => {
			println!("market {market_id}: NO INDEX PRICE (nothing stored)");
			println!("  either no quotes reached the quorum, or the market is not tracked");
			std::process::exit(0)
		},
		Some(raw) => {
			let Some(bytes) = raw.as_str().and_then(hex_bytes) else {
				eprintln!("unexpected result: {raw}");
				std::process::exit(1)
			};
			match PricePoint::<u32>::decode(&mut &bytes[..]) {
				Ok(point) => {
					// ticks × tick_size_quote = quote atomic units; 10^6 of those is one unit.
					let atomic = point.price_ticks as u128 * tick_size_quote;
					let whole = atomic / 1_000_000;
					let frac = atomic % 1_000_000;
					println!("market {market_id}");
					println!("  price_ticks    {}", point.price_ticks);
					println!("  updated_at     block {}", point.updated_at);
					println!("  source_count   {}", point.source_count);
					// Print the fraction too: a sub-unit market (DOGE) would otherwise read
					// as "0 USDT", which looks like a failure.
					println!("  ≈ {whole}.{frac:06} USDT (at {tick_size_quote} atomic per tick)");
				},
				Err(e) => {
					eprintln!("cannot decode a PricePoint from {} bytes: {e}", bytes.len());
					std::process::exit(1)
				},
			}
		},
	}
}
