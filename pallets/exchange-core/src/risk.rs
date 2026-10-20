//! Risk engine: equity, initial/maintenance margin and fill simulation.
//!
//! The engine is a pure function over in-memory state so it can be unit tested without any
//! storage or runtime. All comparisons that involve PnL happen in the signed domain because
//! `equity` is allowed to become negative.

use crate::types::{
    Account, FeePpm, Lots, MarginPpm, MarketPrecision, MathError, PartyState, Position, PriceTicks,
    QuoteAmount, SignedQuote, PPM_DENOM,
};

/// Everything the risk engine needs from a market, in one plain struct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarketParams {
    pub precision: MarketPrecision,
    pub initial_margin_ppm: MarginPpm,
    pub maintenance_margin_ppm: MarginPpm,
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    /// Rebate paid to the maker (§9.3). May exceed `maker_fee_ppm`, which makes the effective
    /// maker fee negative. Paid out of `FeePool`, not out of other users' collateral.
    pub maker_rebate_ppm: FeePpm,
    pub mark_price_ticks: PriceTicks,
    /// Current funding index of the market. Positions are settled against this lazily.
    pub funding_index: SignedQuote,
}

impl MarketParams {
    pub fn maker_fee_for(
        &self,
        price_ticks: PriceTicks,
        lots: Lots,
    ) -> Result<QuoteAmount, MathError> {
        let notional = self.precision.notional(price_ticks, lots)?;
        self.precision.apply_ppm_ceil(notional, self.maker_fee_ppm)
    }

    pub fn taker_fee_for(
        &self,
        price_ticks: PriceTicks,
        lots: Lots,
    ) -> Result<QuoteAmount, MathError> {
        let notional = self.precision.notional(price_ticks, lots)?;
        self.precision.apply_ppm_ceil(notional, self.taker_fee_ppm)
    }
}

/// Immutable snapshot used for all margin maths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskView<'a> {
    pub collateral: QuoteAmount,
    pub fee_debt: QuoteAmount,
    pub reserved: QuoteAmount,
    pub position: &'a Position,
    pub params: MarketParams,
    /// `Σ |notional| × IMR` over the account's **other** markets (§9.2). Zero for a
    /// single-market account; set by the caller, which is the only place that can read storage.
    pub other_initial: QuoteAmount,
    /// Same, with `MMR`.
    pub other_maintenance: QuoteAmount,
}

impl<'a> RiskView<'a> {
    pub fn of(account: &Account, position: &'a Position, params: MarketParams) -> Self {
        Self {
            collateral: account.collateral,
            fee_debt: account.fee_debt,
            reserved: account.reserved_open_order_margin,
            position,
            params,
            other_initial: 0,
            other_maintenance: 0,
        }
    }

    /// Folds in the requirements of the account's other markets (§9.2).
    pub fn with_other_markets(mut self, initial: QuoteAmount, maintenance: QuoteAmount) -> Self {
        self.other_initial = initial;
        self.other_maintenance = maintenance;
        self
    }

    /// Absolute notional of the position valued at the mark price.
    pub fn position_notional(&self) -> Result<QuoteAmount, MathError> {
        self.params.precision.notional(
            self.params.mark_price_ticks,
            self.position.size_lots.unsigned_abs() as Lots,
        )
    }

    pub fn unrealized_pnl(&self) -> Result<SignedQuote, MathError> {
        if self.position.size_lots == 0 {
            return Ok(0);
        }
        let diff = (self.params.mark_price_ticks as i128)
            .checked_sub(self.position.entry_price_ticks as i128)
            .ok_or(MathError::Overflow)?;
        self.params.precision.pnl(self.position.size_lots, diff)
    }

    /// `collateral + realized_pnl + unrealized_pnl - fee_debt`; may be negative.
    pub fn equity(&self) -> Result<SignedQuote, MathError> {
        let base = self.collateral as i128;
        let with_realized = base
            .checked_add(self.position.realized_pnl)
            .ok_or(MathError::Overflow)?;
        let with_unrealized = with_realized
            .checked_add(self.unrealized_pnl()?)
            .ok_or(MathError::Overflow)?;
        with_unrealized
            .checked_sub(self.fee_debt as i128)
            .ok_or(MathError::Overflow)
    }

    /// `|position_notional| * IMR + reserved_open_order_margin`.
    pub fn initial_requirement(&self) -> Result<QuoteAmount, MathError> {
        let position_im = self
            .params
            .precision
            .apply_ppm_ceil(self.position_notional()?, self.params.initial_margin_ppm)?;
        position_im
            .checked_add(self.reserved)
            .and_then(|v| v.checked_add(self.other_initial))
            .ok_or(MathError::Overflow)
    }

    /// `Σ_markets |position_notional| × MMR` (§9.2).
    pub fn maintenance_requirement(&self) -> Result<QuoteAmount, MathError> {
        let own = self.params.precision.apply_ppm_ceil(
            self.position_notional()?,
            self.params.maintenance_margin_ppm,
        )?;
        own.checked_add(self.other_maintenance)
            .ok_or(MathError::Overflow)
    }

    /// `equity >= initial_requirement`. The only condition under which risk may grow.
    pub fn passes_initial_margin(&self) -> Result<bool, MathError> {
        Ok(self.equity()? >= self.initial_requirement()? as SignedQuote)
    }

    /// Absolute position size, used to detect risk-reducing fills.
    pub fn abs_size(&self) -> Lots {
        self.position.size_lots.unsigned_abs()
    }

    /// `equity >= maintenance_requirement`.
    pub fn passes_maintenance_margin(&self) -> Result<bool, MathError> {
        Ok(self.equity()? >= self.maintenance_requirement()? as SignedQuote)
    }
}

/// Result of simulating one fill against one party, before anything is written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FillOutcome {
    pub position: Position,
    pub collateral: QuoteAmount,
    pub reserved: QuoteAmount,
    pub fee: QuoteAmount,
    /// Maker rebate paid out of `FeePool` (§9.3). Zero for takers.
    pub rebate: QuoteAmount,
}

impl PartyState {
    pub fn apply(&mut self, outcome: FillOutcome) {
        self.position = outcome.position;
        self.account.collateral = outcome.collateral;
        self.account.reserved_open_order_margin = outcome.reserved;
    }
}

/// Simulates one fill for one party.
///
/// `signed_fill` is the signed lot delta (`Buy` positive), `is_maker` selects the fee schedule
/// and `reserve_release` is the share of the resting order margin freed by this fill.
/// Returns `Ok(None)` when the fill would leave the party below its initial margin
/// requirement, in which case the caller must not apply it.
pub fn simulate_fill(
    party: &PartyState,
    signed_fill: i64,
    price_ticks: PriceTicks,
    filled_lots: Lots,
    is_maker: bool,
    params: &MarketParams,
    reserve_release: QuoteAmount,
) -> Result<Option<FillOutcome>, MathError> {
    let mut position = party.position;
    crate::settlement::apply_position_delta(
        &mut position,
        signed_fill,
        price_ticks,
        &params.precision,
    )?;

    let fee = if is_maker {
        params.maker_fee_for(price_ticks, filled_lots)?
    } else {
        params.taker_fee_for(price_ticks, filled_lots)?
    };
    // §9.3: only makers are rebated, and only when the tier says so.
    let rebate = if is_maker {
        params.precision.apply_ppm_ceil(
            params.precision.notional(price_ticks, filled_lots)?,
            params.maker_rebate_ppm,
        )?
    } else {
        0
    };

    // An account that cannot even pay the fee (or free its released reserve) cannot take the
    // fill; signal rejection rather than an arithmetic error so the caller can drop the order.
    let collateral = match party
        .account
        .collateral
        .checked_sub(fee)
        .and_then(|c| c.checked_add(rebate))
    {
        Some(c) => c,
        None => return Ok(None),
    };
    let reserved = match party
        .account
        .reserved_open_order_margin
        .checked_sub(reserve_release)
    {
        Some(r) => r,
        None => return Ok(None),
    };

    let view = RiskView {
        other_initial: 0,
        other_maintenance: 0,
        collateral,
        fee_debt: party.account.fee_debt,
        reserved,
        position: &position,
        params: *params,
    };

    let before = RiskView {
        other_initial: 0,
        other_maintenance: 0,
        collateral: party.account.collateral,
        fee_debt: party.account.fee_debt,
        reserved: party.account.reserved_open_order_margin,
        position: &party.position,
        params: *params,
    };
    if !allows_fill(&before, &view)? {
        return Ok(None);
    }

    Ok(Some(FillOutcome {
        position,
        collateral,
        reserved,
        fee,
        rebate,
    }))
}

/// Decides whether the transition from `before` to `after` is acceptable.
///
/// The normal rule is `equity_after >= initial_requirement_after`. A fill that *strictly reduces*
/// the required initial margin without growing the absolute position is additionally allowed even
/// when the account is already underwater, so that a losing account can still close
/// (`reduce_only`) instead of being frozen until Phase 2 liquidation exists.
pub fn allows_fill(before: &RiskView, after: &RiskView) -> Result<bool, MathError> {
    if after.passes_initial_margin()? {
        return Ok(true);
    }
    let before_requirement = before.initial_requirement()?;
    let after_requirement = after.initial_requirement()?;
    Ok(after_requirement < before_requirement && after.abs_size() <= before.abs_size())
}

/// Lots that must be closed to lift a liquidatable account back above maintenance margin.
///
/// Phase 2 §8.2: `q ≥ (N × npl × MMR + B − E) / (a − c + npl × MMR)`, where `npl` is the
/// notional of one lot at the mark price, `a` the PnL realised per closed lot, `c` the
/// liquidation penalty per lot, `B` the safety buffer and `E` the current equity.
///
/// Returns the whole position when closing more would not help: either the denominator is
/// non-positive, or the requirement already exceeds what the account holds.
pub fn liquidation_lots(
    view: &RiskView,
    penalty_ppm: FeePpm,
    buffer_ppm: FeePpm,
    min_lots: Lots,
) -> Result<Lots, MathError> {
    let size = view.position.size_lots;
    if size == 0 {
        return Ok(0);
    }
    let n = size.unsigned_abs();
    let precision = &view.params.precision;

    // Per-lot notional and per-lot maintenance requirement, in quote units.
    let npl = precision.notional(view.params.mark_price_ticks, 1)? as SignedQuote;
    let mm_per_lot = npl
        .checked_mul(view.params.maintenance_margin_ppm as SignedQuote)
        .ok_or(MathError::Overflow)?
        / PPM_DENOM as SignedQuote;

    // `a`: PnL realised by closing one lot. Signed, so a losing position gets a negative
    // contribution — closing more of it makes the account worse, not better.
    let diff = (view.params.mark_price_ticks as SignedQuote)
        .checked_sub(view.position.entry_price_ticks as SignedQuote)
        .ok_or(MathError::Overflow)?;
    let a = precision.pnl(if size > 0 { 1 } else { -1 }, diff)?;
    // `c`: penalty charged per closed lot.
    let c = precision.apply_ppm_ceil(npl as QuoteAmount, penalty_ppm)? as SignedQuote;
    // `B`: safety buffer over the whole position.
    let b = precision.apply_ppm_ceil(
        precision.notional(view.params.mark_price_ticks, n)?,
        buffer_ppm,
    )? as SignedQuote;

    let e = view.equity()?;
    let numerator = mm_per_lot
        .checked_mul(n as SignedQuote)
        .and_then(|v| v.checked_add(b))
        .and_then(|v| v.checked_sub(e))
        .ok_or(MathError::Overflow)?;
    let denominator = a
        .checked_sub(c)
        .and_then(|v| v.checked_add(mm_per_lot))
        .ok_or(MathError::Overflow)?;

    if denominator <= 0 {
        return Ok(n);
    }
    if numerator <= 0 {
        return Ok(min_lots.min(n));
    }
    // Ceil: rounding up closes slightly more, which is the conservative direction.
    let q = (numerator + denominator - 1) / denominator;
    Ok(u64::try_from(q)
        .map_err(|_| MathError::Overflow)?
        .max(min_lots)
        .min(n))
}

/// Conservative margin locked by a resting order:
/// `ceil(notional(limit_price, quantity) * IMR)`.
pub fn order_reserve(
    params: &MarketParams,
    price_ticks: PriceTicks,
    lots: Lots,
) -> Result<QuoteAmount, MathError> {
    let notional = params.precision.notional(price_ticks, lots)?;
    params
        .precision
        .apply_ppm_ceil(notional, params.initial_margin_ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> MarketParams {
        MarketParams {
            precision: MarketPrecision {
                tick_size_quote: 1_000_000,
                lot_size_base: 1_000,
                base_precision: 1_000_000_000,
            },
            initial_margin_ppm: 50_000,
            maintenance_margin_ppm: 25_000,
            maker_fee_ppm: 100,
            taker_fee_ppm: 500,
            maker_rebate_ppm: 0,
            mark_price_ticks: 60_000,
            funding_index: 0,
        }
    }

    #[test]
    fn flat_account_has_zero_requirements() {
        let account = Account::default();
        let position = Position::default();
        let view = RiskView::of(&account, &position, params());
        assert_eq!(view.initial_requirement().unwrap(), 0);
        assert_eq!(view.maintenance_requirement().unwrap(), 0);
        assert!(view.passes_initial_margin().unwrap());
    }

    #[test]
    fn long_position_requires_initial_margin() {
        let mut account = Account::default();
        account.collateral = 1_000_000;
        let mut position = Position::default();
        position.size_lots = 10_000;
        position.entry_price_ticks = 60_000;
        let view = RiskView::of(&account, &position, params());
        // notional = 10_000 * 60_000 = 600_000_000; IM 5% = 30_000_000
        assert_eq!(view.position_notional().unwrap(), 600_000_000);
        assert_eq!(view.initial_requirement().unwrap(), 30_000_000);
        assert!(!view.passes_initial_margin().unwrap());

        account.collateral = 30_000_000;
        let view = RiskView::of(&account, &position, params());
        assert!(view.passes_initial_margin().unwrap());
    }

    #[test]
    fn reserved_margin_counts_towards_initial_requirement() {
        let mut account = Account::default();
        account.collateral = 100_000;
        account.reserved_open_order_margin = 100_000;
        let position = Position::default();
        let view = RiskView::of(&account, &position, params());
        assert_eq!(view.initial_requirement().unwrap(), 100_000);
        assert!(view.passes_initial_margin().unwrap());
    }

    #[test]
    fn equity_can_be_negative_and_fails_maintenance() {
        let account = Account::default();
        let mut position = Position::default();
        position.size_lots = -10_000;
        position.entry_price_ticks = 60_000;
        let mut params = params();
        params.mark_price_ticks = 66_000;
        let view = RiskView::of(&account, &position, params);
        assert_eq!(view.unrealized_pnl().unwrap(), -60_000_000);
        assert_eq!(view.equity().unwrap(), -60_000_000);
        assert!(!view.passes_maintenance_margin().unwrap());
    }

    #[test]
    fn fill_rejected_when_margin_insufficient() {
        let mut account = Account::default();
        account.collateral = 1_000;
        let party = PartyState {
            account,
            position: Position::default(),
        };
        let out = simulate_fill(&party, 1_000, 60_000, 1_000, false, &params(), 0).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn fill_accepted_when_margin_sufficient() {
        let mut account = Account::default();
        account.collateral = 10_000_000;
        let party = PartyState {
            account,
            position: Position::default(),
        };
        let out = simulate_fill(&party, 1_000, 60_000, 1_000, false, &params(), 0).unwrap();
        let out = out.expect("fill must pass");
        assert_eq!(out.position.size_lots, 1_000);
        // notional 60_000_000; taker fee 500 ppm = 30_000
        assert_eq!(out.fee, 30_000);
        assert_eq!(out.collateral, 10_000_000 - 30_000);
    }
}
