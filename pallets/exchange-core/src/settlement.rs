//! Position, realised PnL and fee settlement.
//!
//! Both maker and taker legs go through [`apply_position_delta`] so that there is exactly
//! one settlement implementation and the two sides can never drift apart.

use crate::types::{
    Account, FeePpm, Lots, MarketPrecision, MathError, Position, PriceTicks, QuoteAmount,
    SignedQuote,
};

/// Settles a position against the market's funding index and folds realised PnL into cash.
///
/// This is the Phase 2 §7 semantic change: `realized_pnl` is no longer a permanent side-car
/// that `equity` merely adds — it becomes `collateral`, so a loss actually reduces what the
/// account can withdraw. Without this, two accounts can wash-trade and the loser still
/// withdraws its full nominal collateral (see the H3 hard constraint in the Phase 1 design).
///
/// Returns the amount that could **not** be collected because the account's collateral was
/// insufficient. The caller **must** book that shortfall: funding is zero-sum, so the
/// winning side is credited whether or not the loser can pay. Silently dropping the shortfall
/// would break the custody conservation identity (§4.3).
pub fn settle_position(
    account: &mut Account,
    position: &mut Position,
    funding_index: SignedQuote,
) -> Result<QuoteAmount, MathError> {
    // 1. Funding accrued over the epochs since this position was last touched.
    let owed = if position.funding_index == funding_index {
        0
    } else {
        let owed =
            crate::funding::funding_owed(position.size_lots, funding_index, position.funding_index)
                .map_err(|_| MathError::Overflow)?;
        position.funding_index = funding_index;
        owed
    };

    // 2. Realised PnL becomes cash.
    let realized = position.realized_pnl;
    position.realized_pnl = 0;

    // Positive: the account receives. Negative: it pays.
    let net = realized.checked_sub(owed).ok_or(MathError::Overflow)?;

    if net >= 0 {
        let gain = QuoteAmount::try_from(net).map_err(|_| MathError::Overflow)?;
        account.collateral = account
            .collateral
            .checked_add(gain)
            .ok_or(MathError::Overflow)?;
        Ok(0)
    } else {
        let due = net.unsigned_abs();
        if due <= account.collateral {
            account.collateral -= due;
            Ok(0)
        } else {
            // Take everything that is there and report the rest.
            let unpaid = due - account.collateral;
            account.collateral = 0;
            Ok(unpaid)
        }
    }
}

/// Applies a signed fill of `signed_lots` at `price_ticks` to `position`.
///
/// Returns the realised PnL produced by the fill (zero when the fill opens or increases
/// the position). `position.entry_price_ticks` is always `0` when the resulting size is `0`.
pub fn apply_position_delta(
    position: &mut Position,
    signed_lots: i64,
    price_ticks: PriceTicks,
    precision: &MarketPrecision,
) -> Result<SignedQuote, MathError> {
    if signed_lots == 0 {
        return Ok(0);
    }

    let q = position.size_lots;
    let f = signed_lots;
    let same_direction = q != 0 && (q > 0) == (f > 0);

    if q == 0 || same_direction {
        // Opening or increasing: weighted average entry price, rounded down.
        let abs_q = q.unsigned_abs() as u128;
        let abs_f = f.unsigned_abs() as u128;
        let new_size = q.checked_add(f).ok_or(MathError::Overflow)?;
        if new_size == 0 {
            position.size_lots = 0;
            position.entry_price_ticks = 0;
            return Ok(0);
        }
        let numerator = abs_q
            .checked_mul(position.entry_price_ticks as u128)
            .and_then(|v| v.checked_add(abs_f.checked_mul(price_ticks as u128)?))
            .ok_or(MathError::Overflow)?;
        let denominator = abs_q.checked_add(abs_f).ok_or(MathError::Overflow)?;
        let entry = numerator / denominator;
        position.entry_price_ticks = u64::try_from(entry).map_err(|_| MathError::Overflow)?;
        position.size_lots = new_size;
        Ok(0)
    } else {
        // Closing part or all of the position, possibly reversing it.
        let abs_q = q.unsigned_abs() as u128;
        let abs_f = f.unsigned_abs() as u128;
        let closed = abs_q.min(abs_f);

        let signed_closed = (q.signum() as i128)
            .checked_mul(closed as i128)
            .ok_or(MathError::Overflow)?;
        let closed_i64 = i64::try_from(signed_closed).map_err(|_| MathError::Overflow)?;
        let price_diff = (price_ticks as i128)
            .checked_sub(position.entry_price_ticks as i128)
            .ok_or(MathError::Overflow)?;
        let realised = precision.pnl(closed_i64, price_diff)?;
        position.realized_pnl = position
            .realized_pnl
            .checked_add(realised)
            .ok_or(MathError::Overflow)?;

        let new_size = q.checked_add(f).ok_or(MathError::Overflow)?;
        if new_size == 0 {
            position.size_lots = 0;
            position.entry_price_ticks = 0;
        } else if (new_size > 0) == (q > 0) {
            // Partially closed, direction preserved.
            position.size_lots = new_size;
        } else {
            // Reversed through zero: the remainder opens at the trade price.
            position.size_lots = new_size;
            position.entry_price_ticks = price_ticks;
        }
        Ok(realised)
    }
}

/// `ceil(notional(price, lots) * fee_ppm / 1_000_000)`. Maker fees are never negative.
pub fn fee_amount(
    precision: &MarketPrecision,
    price_ticks: PriceTicks,
    lots: Lots,
    fee_ppm: FeePpm,
) -> Result<QuoteAmount, MathError> {
    let notional = precision.notional(price_ticks, lots)?;
    precision.apply_ppm_ceil(notional, fee_ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prec() -> MarketPrecision {
        MarketPrecision {
            tick_size_quote: 1_000_000,
            lot_size_base: 1_000,
            base_precision: 1_000_000_000,
        }
    }

    #[test]
    fn open_and_increase_averages_entry() {
        let mut p = Position::default();
        assert_eq!(apply_position_delta(&mut p, 10, 100, &prec()).unwrap(), 0);
        assert_eq!(p.size_lots, 10);
        assert_eq!(p.entry_price_ticks, 100);
        assert_eq!(apply_position_delta(&mut p, 10, 200, &prec()).unwrap(), 0);
        assert_eq!(p.size_lots, 20);
        assert_eq!(p.entry_price_ticks, 150);
    }

    #[test]
    fn partial_close_realises_pnl() {
        let mut p = Position::default();
        apply_position_delta(&mut p, 10, 100, &prec()).unwrap();
        // Sell 4 at 150: realised = 4 * (150 - 100) = 200
        assert_eq!(apply_position_delta(&mut p, -4, 150, &prec()).unwrap(), 200);
        assert_eq!(p.size_lots, 6);
        assert_eq!(p.entry_price_ticks, 100);
        assert_eq!(p.realized_pnl, 200);
    }

    #[test]
    fn full_close_zeroes_entry() {
        let mut p = Position::default();
        apply_position_delta(&mut p, 10, 100, &prec()).unwrap();
        assert_eq!(
            apply_position_delta(&mut p, -10, 150, &prec()).unwrap(),
            500
        );
        assert_eq!(p.size_lots, 0);
        assert_eq!(p.entry_price_ticks, 0);
    }

    #[test]
    fn reversal_sets_new_entry() {
        let mut p = Position::default();
        apply_position_delta(&mut p, 10, 100, &prec()).unwrap();
        // Sell 30 at 150: close 10 (+500), then open short 20 at 150.
        assert_eq!(
            apply_position_delta(&mut p, -30, 150, &prec()).unwrap(),
            500
        );
        assert_eq!(p.size_lots, -20);
        assert_eq!(p.entry_price_ticks, 150);
    }

    #[test]
    fn short_side_pnl_is_negative_when_price_rises() {
        let mut p = Position::default();
        apply_position_delta(&mut p, -10, 100, &prec()).unwrap();
        assert_eq!(
            apply_position_delta(&mut p, 10, 150, &prec()).unwrap(),
            -500
        );
        assert_eq!(p.size_lots, 0);
        assert_eq!(p.entry_price_ticks, 0);
    }

    fn account(collateral: QuoteAmount) -> Account {
        let mut a = Account::default();
        a.collateral = collateral;
        a
    }

    #[test]
    fn realised_pnl_is_folded_into_collateral() {
        let mut a = account(1_000);
        let mut p = Position::default();
        p.realized_pnl = -250;
        assert_eq!(settle_position(&mut a, &mut p, 0).unwrap(), 0);
        assert_eq!(a.collateral, 750);
        // The side-car is cleared, so a second settlement is a no-op.
        assert_eq!(p.realized_pnl, 0);
        assert_eq!(settle_position(&mut a, &mut p, 0).unwrap(), 0);
        assert_eq!(a.collateral, 750);
    }

    #[test]
    fn realised_gain_increases_collateral() {
        let mut a = account(1_000);
        let mut p = Position::default();
        p.realized_pnl = 400;
        assert_eq!(settle_position(&mut a, &mut p, 0).unwrap(), 0);
        assert_eq!(a.collateral, 1_400);
    }

    #[test]
    fn funding_is_charged_to_the_paying_side() {
        // Long of 3 in a market whose index rose by 10 per lot pays 30.
        let mut a = account(1_000);
        let mut p = Position::default();
        p.size_lots = 3;
        assert_eq!(settle_position(&mut a, &mut p, 10).unwrap(), 0);
        assert_eq!(a.collateral, 970);
        assert_eq!(p.funding_index, 10);

        // The other side receives it: funding is zero-sum.
        let mut a = account(1_000);
        let mut p = Position::default();
        p.size_lots = -3;
        assert_eq!(settle_position(&mut a, &mut p, 10).unwrap(), 0);
        assert_eq!(a.collateral, 1_030);
    }

    #[test]
    fn settling_twice_charges_only_once() {
        let mut a = account(1_000);
        let mut p = Position::default();
        p.size_lots = 3;
        settle_position(&mut a, &mut p, 10).unwrap();
        settle_position(&mut a, &mut p, 10).unwrap();
        assert_eq!(a.collateral, 970);
    }

    #[test]
    fn funding_and_realised_pnl_net_against_each_other() {
        let mut a = account(1_000);
        let mut p = Position::default();
        p.size_lots = 3;
        p.realized_pnl = 100;
        // owes 30 funding, gains 100 realised -> net +70
        assert_eq!(settle_position(&mut a, &mut p, 10).unwrap(), 0);
        assert_eq!(a.collateral, 1_070);
    }

    #[test]
    fn shortfall_is_reported_not_swallowed() {
        let mut a = account(20);
        let mut p = Position::default();
        p.size_lots = 3;
        // owes 30, only has 20
        assert_eq!(settle_position(&mut a, &mut p, 10).unwrap(), 10);
        assert_eq!(a.collateral, 0);
    }

    #[test]
    fn fee_rounds_up() {
        // notional(100, 1) = 100; 100 * 500 ppm / 1e6 = 0.05 -> ceil = 1
        assert_eq!(fee_amount(&prec(), 100, 1, 500).unwrap(), 1);
        assert_eq!(fee_amount(&prec(), 100, 1_000_000, 500).unwrap(), 50_000);
    }
}
