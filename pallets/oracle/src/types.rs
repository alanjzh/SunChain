//! Core types for `pallet-oracle`.
//!
//! All prices are unsigned integers expressed in `price_ticks`; there is no floating point
//! anywhere in the runtime (Phase 1 design §5.1).

use codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

// Shared with `pallet-exchange-core` so both pallets use the same integer representations.
pub use exchange_primitives::{MarketId, PPM_DENOM, PriceTicks};

/// A whitelisted price source (dYdX v4: a validator running the pricefeed daemon; v8+: Slinky).
#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct SourceRecord<AccountId, BlockNumber> {
	pub key: AccountId,
	pub enabled: bool,
	pub last_submitted_at: BlockNumber,
	/// Monotonic per-source nonce; guards against replayed quotes.
	pub last_nonce: u64,
}

/// One source's latest quote for one market.
#[derive(Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct Quote<BlockNumber> {
	pub price_ticks: PriceTicks,
	pub at: BlockNumber,
	pub nonce: u64,
}

/// The aggregated, on-chain price of a market.
#[derive(Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct PricePoint<BlockNumber> {
	pub price_ticks: PriceTicks,
	pub updated_at: BlockNumber,
	/// Number of fresh sources that contributed to this price.
	pub source_count: u32,
}

/// Median of a non-empty slice, using integer arithmetic only.
///
/// Odd length: the middle element. Even length: `floor((a + b) / 2)` of the two middle
/// elements. The slice is sorted in place.
///
/// dYdX v4 summarises each `funding-sample` epoch with `lib.MustGetMedian` over the premium
/// votes, i.e. the same "median, not mean" choice: it makes a single misbehaving reporter
/// unable to move the value.
pub fn median(sorted: &mut [PriceTicks]) -> Option<PriceTicks> {
	if sorted.is_empty() {
		return None
	}
	sorted.sort_unstable();
	let len = sorted.len();
	if len % 2 == 1 {
		Some(sorted[len / 2])
	} else {
		let a = sorted[len / 2 - 1];
		let b = sorted[len / 2];
		Some(a / 2 + b / 2 + (a % 2 + b % 2) / 2)
	}
}

/// `true` when the relative change between `old` and `new` is at least `min_change_ppm`.
///
/// dYdX v4 `MarketParam.MinPriceChangePpm`: a price update is only written when it moves the
/// price by more than a configured amount. This keeps noise out of state, avoids rewriting
/// (and re-proving) the price on every block, and makes trivial manipulation unprofitable.
pub fn above_min_price_change(
	old: PriceTicks,
	new: PriceTicks,
	min_change_ppm: u32,
) -> bool {
	if old == 0 {
		return true
	}
	let diff = if new > old { new - old } else { old - new };
	(diff as u128)
		.checked_mul(PPM_DENOM)
		.map_or(true, |scaled| scaled >= (old as u128) * (min_change_ppm as u128))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn median_of_odd_and_even_lengths() {
		assert_eq!(median(&mut [5u64]), Some(5));
		assert_eq!(median(&mut [3u64, 1, 2]), Some(2));
		assert_eq!(median(&mut [4u64, 1, 3, 2]), Some(2)); // floor((2+3)/2) = 2
		assert_eq!(median(&mut [10u64, 20]), Some(15));
		assert_eq!(median(&mut []), None);
	}

	#[test]
	fn median_ignores_a_single_outlier() {
		// One source reporting 10x the real price cannot move the median.
		assert_eq!(median(&mut [100u64, 101, 102, 103, 100_000]), Some(102));
	}

	#[test]
	fn min_price_change_gate() {
		// 1% of 60_000 == 600 ticks.
		assert!(!above_min_price_change(60_000, 60_100, 10_000));
		assert!(above_min_price_change(60_000, 60_700, 10_000));
		// Symmetric for downward moves.
		assert!(above_min_price_change(60_000, 59_300, 10_000));
		// A zero previous price always updates (genesis / recovery).
		assert!(above_min_price_change(0, 1, 10_000));
	}
}
