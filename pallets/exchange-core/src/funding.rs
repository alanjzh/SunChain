//! Funding rate computation and lazy accrual (Phase 2, P2-A).
//!
//! Follows dYdX v4:
//!
//! * **Premium** — `P = (max(0, ImpactBid − Index) − max(0, Index − ImpactAsk)) / Index`.
//!   The impact price is the average price at which a notional of
//!   `ImpactNotionalQuote` fills on that side. When the index sits inside the spread, or the
//!   book is too thin, the premium is `0`. A single small order therefore cannot move funding
//!   (`x/clob/memclob.go:GetPricePremium`).
//! * **Two-level epochs** — `funding-sample` takes the **median** of the period's premium
//!   observations; `funding-tick` takes those samples, **removes a tail** from both ends and
//!   averages the rest. Median-then-mean makes one misbehaving period unable to move the rate
//!   (`x/perpetuals/keeper/perpetual.go`).
//! * **Clamp** — `|R| ≤ clamp_factor × (initial margin − maintenance margin)`, i.e. the bound
//!   follows the leverage of the market instead of being a hard-coded percentage.
//! * **Index delta** — `Δ = rate × (Δt / funding period) × notional per lot`, proportional to
//!   elapsed time so an epoch length change cannot over- or under-charge
//!   (`x/perpetuals/funding/funding.go`).
//!
//! All arithmetic is integer-only; division truncates toward zero (Phase 1 §5.1).

use crate::types::{Lots, MarginPpm, PriceTicks, QuoteAmount, SignedQuote, PPM_DENOM};
use sp_core::U256;

/// Errors from the funding math.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FundingError {
    Overflow,
    DivisionByZero,
}

fn to_u128(v: U256) -> Result<u128, FundingError> {
    if v > U256::from(u128::MAX) {
        Err(FundingError::Overflow)
    } else {
        Ok(v.as_u128())
    }
}

/// `(impact − index) * 1e6 / index`, in ppm, truncated toward zero.
///
/// Positive means the perpetual trades above the index (longs pay shorts).
pub fn premium_from_impact(index: PriceTicks, impact: PriceTicks) -> Result<i32, FundingError> {
    if index == 0 {
        return Err(FundingError::DivisionByZero);
    }
    let magnitude = if impact >= index {
        let diff = impact - index;
        let scaled = U256::from(diff)
            .checked_mul(U256::from(PPM_DENOM))
            .ok_or(FundingError::Overflow)?;
        let value = to_u128(scaled / U256::from(index))?;
        i128::try_from(value).map_err(|_| FundingError::Overflow)?
    } else {
        let diff = index - impact;
        let scaled = U256::from(diff)
            .checked_mul(U256::from(PPM_DENOM))
            .ok_or(FundingError::Overflow)?;
        let value = to_u128(scaled / U256::from(index))?;
        -(i128::try_from(value).map_err(|_| FundingError::Overflow)?)
    };
    i32::try_from(magnitude).map_err(|_| FundingError::Overflow)
}

/// Summarises a `funding-tick`: drop `tail_ppm` from both ends after sorting, then average.
///
/// dYdX `processStoredPremiums(..., combineFunc = AvgInt32, filterFunc = removeSampleTails)`.
/// Median (at the sample layer) plus tail removal (here) is what makes the rate robust: one
/// manipulated period is discarded before the average is taken.
///
/// Returns `0` for an empty or fully-trimmed input.
/// Median of a list of observations.
///
/// Used to summarise the premium votes of one sample epoch (P2-D). Unlike the tick
/// average this is **not** trimmed: a single wild vote cannot move a median, which is
/// exactly why votes are summarised this way.
pub fn median(samples: &mut [i32]) -> i32 {
	if samples.is_empty() {
		return 0
	}
	samples.sort_unstable();
	let mid = samples.len() / 2;
	if samples.len() % 2 == 0 {
		let a = samples[mid - 1] as i64;
		let b = samples[mid] as i64;
		((a + b) / 2) as i32
	} else {
		samples[mid]
	}
}

pub fn summarize_samples(samples: &mut [i32], tail_ppm: u32) -> i32 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let len = samples.len();
    // Number of elements removed from *each* end.
    let cut = ((len as u128).saturating_mul(tail_ppm as u128) / PPM_DENOM) as usize;
    if cut.saturating_mul(2) >= len {
        return 0;
    }
    let kept = &samples[cut..len - cut];
    let mut sum: i64 = 0;
    for value in kept {
        sum += *value as i64;
    }
    // Truncate toward zero, matching dYdX `AvgInt32`.
    (sum / kept.len() as i64) as i32
}

/// `|rate| ≤ clamp_factor_ppm × (initial_margin − maintenance_margin) / 1e6`.
///
/// dYdX `liquidityTier.GetMaxAbsFundingClampPpm(params.FundingRateClampFactorPpm)`: the
/// funding a market may charge scales with how much margin it asks for, so a high-leverage
/// market cannot be drained by an extreme rate.
pub fn clamp_rate_ppm(
    rate_ppm: i32,
    initial_margin_ppm: MarginPpm,
    maintenance_margin_ppm: MarginPpm,
    clamp_factor_ppm: u32,
) -> i32 {
    let spread = (initial_margin_ppm as u128).saturating_sub(maintenance_margin_ppm as u128);
    let bound = match spread.checked_mul(clamp_factor_ppm as u128) {
        Some(v) => (v / PPM_DENOM) as i64,
        None => i64::MAX,
    };
    let rate = rate_ppm as i64;
    if rate > bound {
        bound as i32
    } else if rate < -bound {
        -(bound as i32)
    } else {
        rate_ppm
    }
}

/// Change of the funding index for one tick, in quote atomic units **per lot**.
///
/// `Δ = rate_ppm × (elapsed / period) × notional_per_lot / 1e6`, truncated toward zero.
///
/// dYdX `GetFundingIndexDelta`. Being proportional to elapsed time means a shortened or
/// lengthened epoch charges proportionally instead of applying a whole period's rate.
pub fn funding_index_delta(
    rate_ppm: i32,
    elapsed_blocks: u64,
    funding_period_blocks: u64,
    notional_per_lot: QuoteAmount,
) -> Result<SignedQuote, FundingError> {
    if funding_period_blocks == 0 || rate_ppm == 0 || elapsed_blocks == 0 {
        return Ok(0);
    }
    let magnitude = rate_ppm.unsigned_abs() as u128;
    let num = U256::from(magnitude)
        .checked_mul(U256::from(elapsed_blocks as u128))
        .and_then(|v| v.checked_mul(U256::from(notional_per_lot)))
        .ok_or(FundingError::Overflow)?;
    let den = U256::from(funding_period_blocks as u128)
        .checked_mul(U256::from(PPM_DENOM))
        .ok_or(FundingError::Overflow)?;
    let value = to_u128(num / den)?;
    let signed = i128::try_from(value).map_err(|_| FundingError::Overflow)?;
    Ok(if rate_ppm < 0 { -signed } else { signed })
}

/// Funding owed by one position since its last touch.
///
/// `owed = size_lots × (market_index − position_index)`. A long in a rising index pays; a
/// short receives. The caller subtracts `owed` from collateral.
pub fn funding_owed(
    size_lots: i64,
    market_index: SignedQuote,
    position_index: SignedQuote,
) -> Result<SignedQuote, FundingError> {
    let diff = market_index
        .checked_sub(position_index)
        .ok_or(FundingError::Overflow)?;
    (size_lots as i128)
        .checked_mul(diff)
        .ok_or(FundingError::Overflow)
}

/// Maintains the market's net open interest. Call with the signed lot delta of every fill.
pub fn apply_net_oi(net_oi_lots: i64, signed_delta: i64) -> Result<i64, FundingError> {
    net_oi_lots
        .checked_add(signed_delta)
        .ok_or(FundingError::Overflow)
}

/// Convenience for tests / callers: lots are signed.
pub type SignedLots = Lots;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premium_positive_negative_and_zero() {
        // 1% above index.
        assert_eq!(premium_from_impact(60_000, 60_600).unwrap(), 10_000);
        // 1% below index.
        assert_eq!(premium_from_impact(60_000, 59_400).unwrap(), -10_000);
        assert_eq!(premium_from_impact(60_000, 60_000).unwrap(), 0);
        assert!(premium_from_impact(0, 1).is_err());
    }

    #[test]
    fn premium_truncates_toward_zero() {
        // 0.5 ppm rounds down to 0 rather than up.
        assert_eq!(premium_from_impact(1_000_000, 1_000_000 + 1).unwrap(), 1);
        // diff = 1 -> exactly 1 ppm, still truncated (no rounding up).
        assert_eq!(premium_from_impact(1_000_000, 999_999).unwrap(), -1);
        assert_eq!(premium_from_impact(1_000_000, 999_998).unwrap(), -2);
        // Sub-ppm moves become exactly zero, not one.
        assert_eq!(premium_from_impact(10_000_000, 10_000_001).unwrap(), 0);
    }

    #[test]
    fn tail_removal_discards_outliers_before_averaging() {
        // 10% cut from each end removes one element per side from 10 samples.
        let mut samples = [-500i32, 10, 10, 10, 10, 10, 10, 10, 10, 900];
        assert_eq!(summarize_samples(&mut samples, 100_000), 10);

        // Without tail removal the two outliers drag the average to 48 instead of 10.
        let mut raw = [-500i32, 10, 10, 10, 10, 10, 10, 10, 10, 900];
        raw.sort_unstable();
        let plain: i64 = raw.iter().map(|v| *v as i64).sum();
        assert_eq!(plain / raw.len() as i64, 48);
    }

    #[test]
    fn summarize_handles_degenerate_inputs() {
        let mut empty: [i32; 0] = [];
        assert_eq!(summarize_samples(&mut empty, 100_000), 0);
        // Cutting 50% or more from each end leaves nothing; never panic.
        let mut two = [5i32, 7];
        assert_eq!(summarize_samples(&mut two, 500_000), 0);
    }

    #[test]
    fn clamp_follows_margin_spread() {
        // (5% - 2.5%) * 0.6 = 1.5% = 15_000 ppm.
        assert_eq!(clamp_rate_ppm(100_000, 50_000, 25_000, 600_000), 15_000);
        assert_eq!(clamp_rate_ppm(-100_000, 50_000, 25_000, 600_000), -15_000);
        // Inside the bound the rate is untouched.
        assert_eq!(clamp_rate_ppm(1_000, 50_000, 25_000, 600_000), 1_000);
        // A tighter market clamps harder.
        assert_eq!(clamp_rate_ppm(100_000, 20_000, 10_000, 600_000), 6_000);
    }

    #[test]
    fn index_delta_is_proportional_to_elapsed_time() {
        // 8h at 3s blocks = 9600 blocks; rate 1.5% per 8h; notional 60_000 per lot.
        // A full period: 0.015 * 60_000 = 900 per lot.
        assert_eq!(
            funding_index_delta(15_000, 9_600, 9_600, 60_000).unwrap(),
            900
        );
        // A 1h tick (1200 blocks) charges exactly 1/8 of that.
        assert_eq!(
            funding_index_delta(15_000, 1_200, 9_600, 60_000).unwrap(),
            112
        );
        // Negative rate flows the other way, truncated toward zero.
        assert_eq!(
            funding_index_delta(-15_000, 1_200, 9_600, 60_000).unwrap(),
            -112
        );
        // Degenerate inputs.
        assert_eq!(funding_index_delta(0, 1_200, 9_600, 60_000).unwrap(), 0);
        assert_eq!(funding_index_delta(15_000, 0, 9_600, 60_000).unwrap(), 0);
        assert_eq!(funding_index_delta(15_000, 1_200, 0, 60_000).unwrap(), 0);
    }

    #[test]
    fn funding_owed_by_side() {
        // Index rose by 10 per lot: a long of 3 pays 30, a short of 3 receives 30.
        assert_eq!(funding_owed(3, 10, 0).unwrap(), 30);
        assert_eq!(funding_owed(-3, 10, 0).unwrap(), -30);
        assert_eq!(funding_owed(3, 0, 0).unwrap(), 0);
    }

    #[test]
    fn net_oi_tracks_signed_deltas() {
        assert_eq!(apply_net_oi(0, 10).unwrap(), 10);
        assert_eq!(apply_net_oi(10, -4).unwrap(), 6);
        assert_eq!(apply_net_oi(6, -6).unwrap(), 0);
    }
	#[test]
	fn median_of_an_odd_count_is_the_middle() {
		let mut v = [3, 1, 2];
		assert_eq!(median(&mut v), 2);
	}

	#[test]
	fn median_of_an_even_count_averages_the_two_middles() {
		let mut v = [4, 1, 3, 2];
		assert_eq!(median(&mut v), 2, "(2+3)/2 truncates to 2");
	}

	#[test]
	fn median_ignores_a_single_wild_vote() {
		let mut v = [1_000_000, 10, 11];
		assert_eq!(median(&mut v), 11, "one outlier must not move the median");
	}

	#[test]
	fn median_of_nothing_is_zero() {
		assert_eq!(median(&mut []), 0);
	}

}
