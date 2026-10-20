//! The DEX half of genesis: risk tiers, markets, and the oracle that prices them.
//!
//! Shared by the dev spec and the `sunchain` spec. Two copies of "what a market costs"
//! would drift apart, and the failure would be **silent**: one chain would quietly have
//! markets the other does not, until someone tries to place an order and it fails for a
//! reason nobody can see.
//!
//! The oracle **sources** are a parameter, not part of the shared fragment: they are the
//! accounts holding each chain's feed keys, and those genuinely differ per chain.

use sunchain_runtime::AccountId;
use serde_json::json;

/// Byte-array form of an 8-character asset code, which is how the pallet stores it.
fn code(s: &str) -> Vec<u8> {
	let mut out = s.as_bytes().to_vec();
	out.resize(8, b' ');
	out
}

/// Risk tiers and markets for a fresh chain, plus the oracle wiring that prices them.
///
/// Tiers follow dYdX v4 (large-cap 5/3%, mid-cap 10/5%, long-tail 20/10%).
///
/// `tick_size_quote` is per market on purpose: one whole USDT per tick is far too coarse for
/// DOGE. That is exactly why the feed reads it from the chain instead of assuming it.
pub fn dex_genesis(sources: Vec<AccountId>, tracked_markets: Vec<u32>) -> serde_json::Value {
	json!({
		"exchangeCore": {
			"tiers": [
				{
					"name": code("large"),
					"initial_margin_ppm": 50_000,
					"maintenance_margin_ppm": 30_000,
					"impact_notional_quote": 500_000_000
				},
				{
					"name": code("mid"),
					"initial_margin_ppm": 100_000,
					"maintenance_margin_ppm": 50_000,
					"impact_notional_quote": 500_000_000
				},
				{
					"name": code("long"),
					"initial_margin_ppm": 200_000,
					"maintenance_margin_ppm": 100_000,
					"impact_notional_quote": 500_000_000
				}
			],
			"markets": [
				{
					"base_asset_code": code("BTC"),
					"quote_asset_code": code("USDT"),
					"tick_size_quote": 1_000_000,
					"lot_size_base": 100_000,
					"base_precision": 100_000_000,
					"liquidity_tier": 0,
					"maker_fee_ppm": 100,
					"taker_fee_ppm": 500,
					"max_position_lots": 10_000_000
				},
				{
					"base_asset_code": code("ETH"),
					"quote_asset_code": code("USDT"),
					"tick_size_quote": 1_000_000,
					"lot_size_base": 1_000_000,
					"base_precision": 100_000_000,
					"liquidity_tier": 0,
					"maker_fee_ppm": 100,
					"taker_fee_ppm": 500,
					"max_position_lots": 10_000_000
				},
				{
					"base_asset_code": code("SOL"),
					"quote_asset_code": code("USDT"),
					"tick_size_quote": 10_000,
					"lot_size_base": 10_000_000,
					"base_precision": 100_000_000,
					"liquidity_tier": 1,
					"maker_fee_ppm": 100,
					"taker_fee_ppm": 500,
					"max_position_lots": 10_000_000
				},
				{
					"base_asset_code": code("DOGE"),
					"quote_asset_code": code("USDT"),
					"tick_size_quote": 1_000,
					"lot_size_base": 100_000_000,
					"base_precision": 100_000_000,
					"liquidity_tier": 2,
					"maker_fee_ppm": 100,
					"taker_fee_ppm": 500,
					"max_position_lots": 10_000_000
				}
			]
		},
		"oracle": {
			"sources": sources,
			"trackedMarkets": tracked_markets
		}
	})
}

/// Merges the DEX fragment into an existing genesis patch.
pub fn merge_dex_genesis(genesis: &mut serde_json::Value, sources: Vec<AccountId>, tracked_markets: Vec<u32>) {
	let fragment = dex_genesis(sources, tracked_markets);
	let object = genesis.as_object_mut().expect("genesis is a JSON object");
	for (key, value) in fragment.as_object().expect("DEX genesis is a JSON object") {
		object.insert(key.clone(), value.clone());
	}
}
