//! Cross-pallet traits.
//!
//! These are defined here — not in the consuming pallet — so the dependency arrows stay
//! strictly one-way and no two pallets need to depend on each other's crate:
//!
//! ```text
//! pallet-oracle       implements PriceOracle
//! pallet-insurance    implements InsuranceFund
//! pallet-exchange-core consumes both
//! ```

use crate::types::{PriceTicks, SignedQuote};
use sp_std::vec::Vec;
use codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_runtime::DispatchResult;

/// Read-only snapshot used to rank an account as an ADL candidate (§8.5).
///
/// Purely in-memory: it crosses the pallet boundary but is never stored, so it needs no SCALE
/// representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdlRank {
	/// Signed position. `0` means the account is not a candidate at all.
	pub size_lots: i64,
	/// Unrealised PnL at the current mark. ADL only ever touches profitable accounts.
	pub unrealized_pnl: SignedQuote,
	/// Return on equity in ppm.
	pub roe_ppm: i64,
	/// `P_mark_adjusted`: the effective mark price this ranking was computed against
	/// (§8.5.1). Carried here so the ADL execution price is derived from the *same* price the
	/// candidate selection used.
	pub mark_price_ticks: PriceTicks,
	/// Notional of a single lot at `mark_price_ticks`, in quote units.
	///
	/// Needed to size a *partial* ADL: the callers know how much exposure they want to shed in
	/// quote terms, but `adl_close` takes lots.
	pub notional_per_lot: crate::types::QuoteAmount,
}

/// Losses that collateral, liquidation proceeds and the insurance fund could not cover.
///
/// §8.5.1: recorded explicitly, never implicitly socialised. ADL does not reduce it — only the
/// insurance fund covering it, or governance, may.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct ResidualLoss {
	pub market_id: crate::types::MarketId,
	/// Sign of the position that could not be closed: `+1` long, `-1` short, `0` unknown.
	pub side_sign: i64,
	pub amount: crate::types::QuoteAmount,
	/// Exposure ADL has already shed against this loss.
	///
	/// Separate from `amount`: §8.5.1 §2 says ADL reduces exposure, not the recorded loss.
	/// This is only the budget that stops one loss event licensing unlimited ADL.
	pub shed_notional: crate::types::QuoteAmount,}

/// Prices a risk engine needs from the oracle layer.
///
/// Modelled on dYdX v4, which keeps a **single** on-chain price per market: the index price
/// (median over exchange feeds) is used directly for margin, unrealised PnL, funding and
/// liquidation. There is no separate "mark price" and the last traded price is never used.
///
/// The two accessors are kept separate so a later revision can introduce clamping or
/// smoothing between them without touching any call site. Until then
/// `mark_price == index_price`.
pub trait PriceOracle<MarketId> {
	/// Median over all fresh source quotes. `None` when the quorum is not met or the last
	/// update is too old.
	fn index_price(market: MarketId) -> Option<PriceTicks>;

	/// Price used for margin, unrealised PnL and liquidation. Equals [`Self::index_price`]
	/// today; never derived from the last traded price.
	fn mark_price(market: MarketId) -> Option<PriceTicks>;

	/// Whether the price is fresh enough to *grow* risk (open positions, place orders,
	/// withdraw) and to trigger a liquidation.
	///
	/// When this returns `false` the core must refuse to liquidate: liquidating at a stale or
	/// unknown price would dispose of user collateral at an arbitrary valuation. This is also
	/// the gate that satisfies "禁止使用最后成交价直接触发清算" and "预言机超时时市场进入受限
	/// 状态" (discussion doc §9).
	fn is_fresh(market: MarketId) -> bool;
}

/// Placeholder used before `pallet-oracle` is wired up: unit tests, and a runtime that has not
/// registered the oracle pallet yet.
///
/// It reports "no price", which makes `is_fresh()` false — so the core refuses to liquidate
/// and refuses to let a market run without a price. `pallet-exchange-core` keeps its
/// Root-set cached price as an explicit testnet fallback, so Phase 1 behaviour is preserved
/// until the oracle goes live.
pub struct NoOracle;

impl<M> PriceOracle<M> for NoOracle {
	fn index_price(_market: M) -> Option<PriceTicks> {
		None
	}
	fn mark_price(_market: M) -> Option<PriceTicks> {
		None
	}
	fn is_fresh(_market: M) -> bool {
		false
	}
}

/// Insurance fund: absorbs bad debt and collects liquidation penalties.
///
/// Phase 2 design §8.4. The fund is spent **before** any loss is socialised to profitable
/// traders (ADL); skipping the fund is a protocol violation, not a policy choice.
pub trait InsuranceFund<Balance> {
	/// Covers `shortfall` from the fund. Implementations may cover it in several steps; the
	/// caller must treat a returned error as "fund exhausted" and fall through to ADL.
	fn cover(shortfall: Balance) -> DispatchResult;

	/// Credits the fund (liquidation penalties, fee share, funding residual).
	fn credit(surplus: Balance) -> DispatchResult;

	/// Currently available balance.
	fn available() -> Balance;
}

/// Controlled entry points `pallet-exchange-core` exposes to `pallet-liquidation`.
///
/// Phase 2 design §4.2: the liquidation pallet must never reach into the matching internals;
/// it may only drive these two operations. Both are bounded and both go through the same
/// risk engine as a normal fill.
pub trait ExchangeEngine<AccountId> {
	/// Closes `lots` of `who`'s position in `market` at the given limit price, behaving like a
	/// reduce-only IOC order. Returns the lots actually closed.
	/// `keeper` is who submitted the liquidation and will be paid the rebate. It is recorded
	/// per keeper so that one keeper can never claim another's pending rebate.
	fn liquidate_account(
		keeper: &AccountId,
		who: &AccountId,
		subaccount: crate::types::SubaccountId,
		market: crate::types::MarketId,
		lots: crate::types::Lots,
	)
		-> Result<crate::types::Lots, sp_runtime::DispatchError>;

	/// Offsets a bankrupt account's position against the opposite side at `price`.
	fn adl_close(
		who: &AccountId,
		subaccount: crate::types::SubaccountId,
		market: crate::types::MarketId,
		lots: crate::types::Lots,
		price: PriceTicks,
	) -> Result<(), sp_runtime::DispatchError>;

	/// Pays the accrued liquidation rebate to the keeper who submitted the liquidation and
	/// returns the amount paid. Capped by what previous liquidations actually accrued, so a
	/// keeper can never be paid out of user collateral.
	fn pay_keeper(keeper: &AccountId) -> Result<crate::types::QuoteAmount, sp_runtime::DispatchError>;

	/// The sub-accounts `who` has used, for the liquidation pallet to enumerate when it must
	/// touch every sub-account (S3-1). Always includes the default sub-account.
	fn subaccounts(who: &AccountId) -> Vec<crate::types::SubaccountId>;

	/// Read-only ranking of `who`'s `subaccount` as an ADL candidate. `None` when that
	/// sub-account has no position in the market.
	fn adl_rank(
		who: &AccountId,
		subaccount: crate::types::SubaccountId,
		market: crate::types::MarketId,
	) -> Option<AdlRank>;

	/// Explicitly recorded residual loss attributed to `who`'s `subaccount` (§8.5.1 §2).
	/// `amount == 0` means there is nothing for ADL to respond to.
	fn residual_loss_of(who: &AccountId, subaccount: crate::types::SubaccountId) -> ResidualLoss;

	/// Records `notional` of exposure that ADL has shed against a residual loss.
	///
	/// ADL is meant to remove exposure worth the uncovered loss — no more, and exactly once.
	/// The loss itself is left alone (§8.5.1 §2); only the remaining budget shrinks, so a
	/// single loss event cannot license ADL forever.
	fn note_adl_shed(
		who: &AccountId,
		subaccount: crate::types::SubaccountId,
		notional: crate::types::QuoteAmount,
	);
}
