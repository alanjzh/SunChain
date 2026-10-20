//! Converting between real-world prices and the runtime's `PriceTicks`.
//!
//! The runtime never stores a price "in dollars". `Market::tick_size_quote` fixes how many
//! quote atomic units one tick is worth, and every price in the system is a whole number of
//! ticks. Anything entering from outside — an off-chain price feed above all — has to be put
//! on that scale.
//!
//! Getting this wrong multiplies every price, and therefore every funding payment and every
//! liquidation, by a constant. It does not error, it does not look suspicious: the numbers
//! simply are what they are. So it lives here, once, with tests, rather than being re-derived
//! at each call site.

use crate::{PriceTicks, QuoteAmount};

/// The scale our off-chain price feed reports: millionths of one whole quote unit.
pub const FEED_SCALE: u32 = 1_000_000;

/// Quote atomic units per price tick. Immutable once a market is created.
pub type TickSizeQuote = QuoteAmount;

fn pow10(exp: u32) -> Option<u128> {
	10u128.checked_pow(exp)
}

/// Converts an off-chain price into [`PriceTicks`].
///
/// * `price_scaled` — the price in `scale`-ths of one whole quote unit. The feed uses
///   [`FEED_SCALE`] (millionths), so `$76_908.50` arrives as `76_908_500_000`.
/// * `quote_decimals` — the quote asset's decimals (6 for USDT and USDC). A quote batch is
///   `10^quote_decimals` atomic units.
/// * `tick_size_quote` — the market's tick size, already in atomic units.
///
/// Rounds to the nearest tick; returns `None` rather than saturating when anything would
/// overflow or does not fit in a [`PriceTicks`], so a caller cannot quietly publish `u64::MAX`.
pub fn price_to_ticks(
	price_scaled: u128,
	scale: u32,
	quote_decimals: u8,
	tick_size_quote: TickSizeQuote,
) -> Option<PriceTicks> {
	if tick_size_quote == 0 || scale == 0 {
		return None
	}
	// `scale` is the divisor itself (a price of `P` in millionths means `P / 1_000_000`
	// whole units); `quote_decimals` is an exponent, giving `10^decimals` atomic units.
	let from = scale as u128;
	let to = pow10(quote_decimals as u32)?;

	// Re-scale the price from the feed's units into quote atomic units.
	let price_atomic = price_scaled.checked_mul(to)?.checked_div(from)?;

	// Nearest tick rather than floor: halving the error is free.
	let ticks = price_atomic
		.checked_add(tick_size_quote / 2)?
		.checked_div(tick_size_quote)?;

	u64::try_from(ticks).ok()
}

/// Inverse of [`price_to_ticks`]: recovers a feed-scale price from ticks.
///
/// Used for display and diagnostics, and by the round-trip test below. It cannot recover
/// rounding — a conversion is lossy by up to half a tick — only scale.
pub fn ticks_to_price(
	ticks_: PriceTicks,
	scale: u32,
	quote_decimals: u8,
	tick_size_quote: TickSizeQuote,
) -> Option<u128> {
	if scale == 0 {
		return None
	}
	let from = scale as u128;
	let to = pow10(quote_decimals as u32)?;

	let price_atomic = (ticks_ as u128).checked_mul(tick_size_quote)?;
	price_atomic.checked_mul(from)?.checked_div(to).into()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// USDT / USDC.
	const SIX: u8 = 6;

	#[test]
	fn a_usdt_price_in_millionths_maps_onto_a_usdt_tick_size() {
		// Tick size: exactly one whole USDT per tick.
		let tick = 1_000_000u128;
		// BTC at 76_908.50 USDT, as the feed reports it.
		let ticks = price_to_ticks(76_908_500_000, FEED_SCALE, SIX, tick).unwrap();
		// 76_908.5 rounds to nearest tick.
		assert_eq!(ticks, 76_909);
	}

	#[test]
	fn a_finer_tick_size_keeps_more_of_the_price() {
		// One hundred-thousandth of a USDT per tick: 0.000_01 USDT granularity.
		let tick = 10u128;
		let ticks = price_to_ticks(76_908_500_000, FEED_SCALE, SIX, tick).unwrap();
		assert_eq!(ticks, 7_690_850_000);
	}

	#[test]
	fn round_trip_recovers_the_price_to_within_one_tick() {
		let tick = 1_000_000u128;
		let price = 76_908_500_000u128;
		let ticks = price_to_ticks(price, FEED_SCALE, SIX, tick).unwrap();
		let back = ticks_to_price(ticks, FEED_SCALE, SIX, tick).unwrap();
		let difference = if back > price { back - price } else { price - back };
		assert!(difference <= tick * FEED_SCALE as u128, "off by {difference}, more than one tick");
	}

	/// The parameter that is easiest to get wrong, and whose error is invisible.
	#[test]
	fn quote_decimals_matters_by_a_factor_of_ten_to_the_twelfth() {
		// A price of 1.0, and a tick of one atomic unit so nothing else intervenes.
		let price = FEED_SCALE as u128;
		let eighteen = price_to_ticks(price, FEED_SCALE, 18, 1).unwrap();
		let six = price_to_ticks(price, FEED_SCALE, SIX, 1).unwrap();
		assert_eq!(six * 1_000_000_000_000, eighteen);
	}

	#[test]
	fn a_zero_tick_size_is_refused_rather_than_divided_by() {
		assert_eq!(price_to_ticks(100, FEED_SCALE, SIX, 0), None);
	}

	#[test]
	fn a_price_that_does_not_fit_is_refused_rather_than_truncated() {
		// Far beyond u64; must not wrap or saturate into a plausible-looking price.
		assert_eq!(price_to_ticks(u128::MAX, FEED_SCALE, SIX, 1), None);
	}

	#[test]
	fn a_zero_price_stays_zero_and_the_oracle_rejects_it_later() {
		assert_eq!(price_to_ticks(0, FEED_SCALE, SIX, 1_000_000), Some(0));
	}
}
