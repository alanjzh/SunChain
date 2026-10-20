//! Scalar types shared by every exchange pallet.
//!
//! These live here so `pallet-oracle`, `pallet-exchange-core`, `pallet-insurance` and
//! `pallet-liquidation` all agree on the same integer representations. `pallet-exchange-core`
//! re-exports them from its own `types` module, so existing imports keep working.
//!
//! Every monetary value is an integer or a fixed point integer; the runtime never uses
//! `f32` / `f64` (Phase 1 design §5.1).

pub type MarketId = u32;

/// Sub-account within an account (§9.1).
///
/// The key dimension is introduced in Phase 2 but only `DEFAULT_SUBACCOUNT` is used, so that
/// Phase 3 does not need another full storage migration.
pub type SubaccountId = u32;

/// The only sub-account Phase 2 populates. See [`SubaccountId`].
pub const DEFAULT_SUBACCOUNT: SubaccountId = 0;
pub type PriceTicks = u64;
pub type Lots = u64;
pub type QuoteAmount = u128;
pub type SignedQuote = i128;
pub type FeePpm = u32;
pub type MarginPpm = u32;

/// Parts-per-million denominator. `1_000_000` == 100%.
pub const PPM_DENOM: u128 = 1_000_000u128;
