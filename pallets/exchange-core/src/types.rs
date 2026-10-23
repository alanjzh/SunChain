//! Core SCALE types and deterministic integer arithmetic for `pallet-exchange-core`.
//!
//! Every monetary value is an integer or a fixed point integer. `f32`/`f64` are never used.
//! All multiplications are carried out in `U256` and the result must provably fit into `u128`,
//! otherwise `MathError::Overflow` is returned (never saturating arithmetic).

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_core::hashing::blake2_256;
use sp_core::U256;

// ----------------------------------------------------------------------------
// Scalar aliases
// ----------------------------------------------------------------------------

/// Scalar types shared with `pallet-oracle` / `pallet-insurance`; see
/// `pallets/exchange-primitives`. Re-exported here so existing `crate::types::*` imports keep
/// working.
pub use exchange_primitives::{
    FeePpm, Lots, MarginPpm, MarketId, PriceTicks, QuoteAmount, SignedQuote, SubaccountId,
    DEFAULT_SUBACCOUNT, PPM_DENOM,
};

pub type OrderId = u64;

/// Identifier of a fee tier (§9.3).
pub type FeeTierId = u32;

/// A fee tier. `maker_rebate_ppm` above `maker_fee_ppm` makes the effective maker fee
/// negative — the maker is *paid* to post liquidity.
///
/// The rebate is paid out of `FeePool`, so it participates in the custody conservation
/// identity; see `Pallet::charge_maker_rebate` for the `InsufficientFeePool` guard.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    Encode,
    Decode,
    DecodeWithMemTracking,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct FeeTier {
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    /// Rebate paid to the maker; may exceed `maker_fee_ppm` (negative effective fee).
    pub maker_rebate_ppm: FeePpm,
}
pub type ClientOrderId = u64;

/// Errors produced by the pure integer math layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MathError {
    /// A `U256` intermediate value did not fit into `u128`/`i128`.
    Overflow,
    /// Division by zero (only reachable with a mis-configured market).
    DivisionByZero,
}

// ----------------------------------------------------------------------------
// Enums
// ----------------------------------------------------------------------------

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum TimeInForce {
    GoodTilCancelled,
    ImmediateOrCancel,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum MarketStatus {
    Active,
    ReduceOnly,
    CancelOnly,
    Paused,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum AccountStatus {
    Normal,
    Restricted,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum OrderStatus {
    Open,
    Filled,
    Cancelled,
    Expired,
    RiskCancelled,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum CancelReason {
    User,
    IocRemainder,
    Expired,
    Risk,
    NonceInvalidated,
    MarketPaused,
    MatchLimitReached,
}

// ----------------------------------------------------------------------------
// Market
// ----------------------------------------------------------------------------

/// Precision + risk + fee parameters of a market.
///
/// `tick_size_quote`, `lot_size_base` and `base_precision` are immutable once the market
/// holds orders or positions.
#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct Market<BlockNumber> {
    /// Display only, e.g. `BTC`.
    pub base_asset_code: [u8; 8],
    /// Display only, e.g. `USD`.
    pub quote_asset_code: [u8; 8],
    /// Quote (USDC) atomic units per price tick.
    pub tick_size_quote: QuoteAmount,
    /// Base atomic units per lot.
    pub lot_size_base: u64,
    /// Base atomic units per whole base unit.
    pub base_precision: u64,
    /// Resolved copy of the tier's margins. The maths reads these; they are refreshed
    /// whenever the tier changes.
    pub initial_margin_ppm: MarginPpm,
    pub maintenance_margin_ppm: MarginPpm,
    /// The tier these margins came from, so a tier update can find them again.
    pub liquidity_tier: u32,
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    pub max_position_lots: Lots,
    pub status: MarketStatus,
    pub best_bid: Option<PriceTicks>,
    pub best_ask: Option<PriceTicks>,
    pub bid_level_count: u32,
    pub ask_level_count: u32,
    /// Sum of every open position in this market, signed (long positive).
    ///
    /// The funding paid by one side does not necessarily equal what the other side receives
    /// when net OI is non-zero; the difference is the residual absorbed by the insurance
    /// fund (Phase 2 design §6.3).
    pub net_oi_lots: i64,
    /// Cumulative funding index, in quote atomic units **per lot**.
    pub funding_index: SignedQuote,
    pub last_funding_at: BlockNumber,
    /// Funding rate applied at the last tick, ppm. Informational (UI / indexer).
    pub rate_ppm: i32,
    /// Monotonic order id source. Order ids must be unique per market, so a per-account
    /// counter cannot be used for the `(market_id, order_id)` storage key.
    pub next_order_id: OrderId,
}

impl<BlockNumber> Market<BlockNumber> {
    /// Precision bundle used by the math layer.
    pub fn precision(&self) -> MarketPrecision {
        MarketPrecision {
            tick_size_quote: self.tick_size_quote,
            lot_size_base: self.lot_size_base,
            base_precision: self.base_precision,
        }
    }
}

/// Risk parameters shared by several markets (dYdX v4 `LiquidityTier`).
///
/// v4 groups initial margin, maintenance margin and impact notional into a *tier* that many
/// markets point at, so a risk parameter is changed in exactly one place. We list the same
/// three per tier rather than per market.
///
/// Deviation from v4, deliberate: v4's `MaintenanceFractionPpm` is a **fraction of the
/// initial margin** (so the two always move together). Ours is absolute, because the
/// liquidation maths here was written against absolute values; changing that would be a
/// separate, riskier change.
#[derive(serde::Serialize, serde::Deserialize)]
#[derive(
    Clone, Copy, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub struct LiquidityTier {
    pub name: [u8; 8],
    pub initial_margin_ppm: MarginPpm,
    pub maintenance_margin_ppm: MarginPpm,
    /// Notional used to derive the impact price for funding (dYdX `ImpactNotional`).
    pub impact_notional_quote: QuoteAmount,
}

/// Parameters accepted by `create_market`.
#[derive(serde::Serialize, serde::Deserialize)]
#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub struct MarketConfig {
    pub base_asset_code: [u8; 8],
    pub quote_asset_code: [u8; 8],
    pub tick_size_quote: QuoteAmount,
    pub lot_size_base: u64,
    pub base_precision: u64,
    /// Which [`LiquidityTier`] supplies this market's margins.
    pub liquidity_tier: u32,
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    pub max_position_lots: Lots,
}

/// Parameters accepted by `set_risk_params`.
#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub struct RiskParams {
    pub initial_margin_ppm: MarginPpm,
    pub maintenance_margin_ppm: MarginPpm,
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    pub max_position_lots: Lots,
}

// ----------------------------------------------------------------------------
// Accounts and positions
// ----------------------------------------------------------------------------

#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub struct Account {
    /// Free collateral credited to the exchange core custody account.
    pub collateral: QuoteAmount,
    /// Margin currently locked by resting orders.
    pub reserved_open_order_margin: QuoteAmount,
    /// Reserved for future upgrades; Phase 1 always zero.
    pub fee_debt: QuoteAmount,
    /// All resting orders with `client_order_id < invalid_before` are logically dead.
    pub invalid_before: ClientOrderId,
    pub open_order_count: u32,
    pub status: AccountStatus,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            collateral: 0,
            reserved_open_order_margin: 0,
            fee_debt: 0,
            invalid_before: 0,
            open_order_count: 0,
            status: AccountStatus::Normal,
        }
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct Position {
    /// Positive = long, negative = short.
    pub size_lots: i64,
    /// Must be `0` when `size_lots == 0`.
    pub entry_price_ticks: PriceTicks,
    pub realized_pnl: SignedQuote,
    /// Value of `Market::funding_index` when this position was last touched. Funding is
    /// settled lazily as `size_lots * (market_index - funding_index)`.
    ///
    /// NOTE: adding this field changes the SCALE encoding of `Position`. A migration is
    /// required before any chain that already holds Phase 1 state is upgraded
    /// (Phase 2 design §10).
    pub funding_index: SignedQuote,
}

impl Position {
    pub fn is_flat(&self) -> bool {
        self.size_lots == 0
    }

    /// How many lots a `reduce_only` order on `side` may still close.
    pub fn reduce_capacity(&self, side: Side) -> Lots {
        match side {
            Side::Buy => {
                if self.size_lots < 0 {
                    self.size_lots.unsigned_abs()
                } else {
                    0
                }
            }
            Side::Sell => {
                if self.size_lots > 0 {
                    self.size_lots.unsigned_abs()
                } else {
                    0
                }
            }
        }
    }
}

/// In-memory (per dispatch) view of one trading party.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartyState {
    pub account: Account,
    pub position: Position,
}

// ----------------------------------------------------------------------------
// Orders, requests, actions
// ----------------------------------------------------------------------------

#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(AccountId, BlockNumber))]
pub struct Order<AccountId, BlockNumber> {
    pub owner: AccountId,
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub original_lots: Lots,
    pub remaining_lots: Lots,
    pub reserved_margin: QuoteAmount,
    pub tif: TimeInForce,
    pub post_only: bool,
    pub reduce_only: bool,
    pub placed_at: BlockNumber,
    pub expires_at: BlockNumber,
    pub prev_order: Option<OrderId>,
    pub next_order: Option<OrderId>,
}

#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct OrderRequest<BlockNumber> {
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub quantity_lots: Lots,
    pub tif: TimeInForce,
    pub post_only: bool,
    pub reduce_only: bool,
    pub expires_at: BlockNumber,
}

/// One bounded action of `execute_batch`. SCALE stable for Phase B.
#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(BlockNumber))]
pub enum ExchangeAction<BlockNumber> {
    Place(OrderRequest<BlockNumber>),
    Cancel(ClientOrderId),
    CancelUpTo(ClientOrderId),
}

impl<BlockNumber> ExchangeAction<BlockNumber> {
    /// Priority class (§12.3). Non-risk actions settle first, then cancels, then places:
    /// an account must be able to shed exposure before it is asked to take on more.
    pub fn class(&self) -> ActionClass {
        match self {
            ExchangeAction::Place(_) => ActionClass::Place,
            ExchangeAction::Cancel(_) | ExchangeAction::CancelUpTo(_) => ActionClass::Cancel,
        }
    }
}

/// Priority class of an action. The discriminant order *is* the priority (§12.3).
#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    TypeInfo,
    MaxEncodedLen,
)]
pub enum ActionClass {
    /// Non-risk: settlement, funding updates.
    Other = 0,
    Cancel = 1,
    Place = 2,
}

/// Deterministic ordering key (§12.3, S1-3): `(class, account, action_nonce, action_hash)`.
///
/// Every component is signed, or derived from signed bytes, so the proposer that assembled
/// the batch cannot change the order by rearranging it. It used to end in the position in the
/// batch — which is exactly the thing the proposer chose, and the reason "the batch order is
/// deterministic" was true while "the proposer cannot reorder" was not.
///
/// `account` hashes `(signer, subaccount)`, which groups one account's actions together. That
/// is what makes several actions from one account run in nonce order: ordering by class alone
/// could put a higher nonce first, and the second action would then be refused for a nonce it
/// did not choose.
#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct SortKey {
    pub class: ActionClass,
    pub account: [u8; 32],
    pub action_nonce: u64,
    pub action_hash: [u8; 32],
}

/// Domain tag for the bytes an action's signature covers (S1-1).
///
/// The payload used to be `(subaccount, action_nonce, action)`, which binds nothing: the
/// same bytes verify on any chain, in any environment, for as long as the nonce has not been
/// passed. A domain tag makes the *meaning* of the payload part of the payload, so a format
/// change cannot be mistaken for a reuse.
pub const SIGNED_ACTION_DOMAIN: &[u8] = b"sunchain:trade-action";

/// Version of the signed-action domain (S1-1).
///
/// Bumping it invalidates every outstanding signature on purpose: the alternative is two
/// payload formats that both "verify", which is how a stale client keeps signing forever.
pub const SIGNED_ACTION_VERSION: u16 = 1;

/// What became of one action in a batch (S1-5).
///
/// A batch reports `Ok` whether or not it executed anything, so without this the only way to
/// tell "applied" from "refused" was to read the book and infer. Each variant says whether the
/// nonce was consumed, which is the part a client cannot work out on its own: a refused action
/// leaves the nonce free, a rejected one spends it.
#[derive(
    Clone, Copy, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub enum ActionOutcome {
    /// Ran and committed.
    Applied,
    /// Refused before anything ran — bad signature, expired, wrong sub-account, nonce not
    /// ahead. The nonce was **not** consumed.
    Refused,
    /// Ran and was rolled back (S1-2): the business part wrote nothing, but the nonce was
    /// **consumed**, so the same signature cannot be replayed.
    Rejected,
}

impl ActionOutcome {
    /// Whether the action's nonce was spent. True for everything that got as far as the
    /// nonce check.
    pub fn consumed_nonce(self) -> bool {
        !matches!(self, ActionOutcome::Refused)
    }
}

/// One action's receipt: what happened, when, and why it did not (S1-5).
///
/// Kept per `(account, sub-account)` as a small ring of the most recent outcomes, so a client
/// can answer "did my order run?" without replaying the chain. Bounded by
/// `MaxReceiptsPerAccount` — an unbounded receipt log would be a storage growth vector, and
/// the whole point is to make the *existing* silence observable, not to add a new liability.
#[derive(
    Clone, Copy, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
pub struct ActionReceipt {
    pub action_nonce: u64,
    /// Block the batch that carried it was executed in.
    pub block: u32,
    pub outcome: ActionOutcome,
    /// The pallet's error code, when it did not apply: `(pallet index, error code)`. `None`
    /// for an applied action.
    ///
    /// A `DispatchError` is not storable — its `Token` variant carries an unbounded payload —
    /// so the two numbers that identify the error are kept instead. They decode to the same
    /// variant through the pallet's metadata.
    pub error: Option<(u8, [u8; 4])>,
}

/// One user-signed action in a proposer-built `Exchange Batch` (§12.4).
///
/// `signer` + `signature` are what authorise it — never the account that submitted the
/// batch. `action_nonce` is per `(signer, subaccount)` and must increase strictly.
#[derive(
    Clone, Debug, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, TypeInfo, MaxEncodedLen,
)]
#[scale_info(skip_type_params(BlockNumber))]
pub struct SignedExchangeAction<Signer, Signature, BlockNumber> {
    pub signer: Signer,
    pub subaccount: exchange_primitives::SubaccountId,
    pub action_nonce: u64,
    /// When the signature stops being valid, in blocks (S1-1).
    ///
    /// The nonce alone is not an expiry: a cancel signed today would stay valid for as long
    /// as the nonce had not been passed, which for a leaked signature is forever. This
    /// bounds the window for Cancel and CancelUpTo exactly as `OrderRequest::expires_at`
    /// bounds Place — the two are independent, and both are signed.
    pub expires_at: BlockNumber,
    pub action: ExchangeAction<BlockNumber>,
    pub signature: Signature,
}

impl<Signer: Encode, Signature: Encode, BlockNumber: Encode> SignedExchangeAction<Signer, Signature, BlockNumber>
where
    BlockNumber: Clone,
{
    /// The canonical position of this action in a batch (S1-3).
    ///
    /// Derived from the action's own bytes, so it is the same for every validator and no
    /// proposer can influence it. See [`SortKey`]. The signature is deliberately *not* part
    /// of the hash: two actions with the same signer, sub-account, nonce and expiry are the
    /// same action, and the strict-ascending check in `validate_unsigned` refuses the pair.
    pub fn sort_key(&self) -> SortKey {
        SortKey {
            class: self.action.class(),
            account: blake2_256(&(self.signer.encode(), self.subaccount).encode()),
            action_nonce: self.action_nonce,
            action_hash: blake2_256(
                &(
                    self.signer.encode(),
                    self.subaccount,
                    self.action_nonce,
                    self.expires_at.clone(),
                    &self.action,
                )
                    .encode(),
            ),
        }
    }
}

/// One price level of the order book: a doubly linked FIFO queue of orders plus the
/// adjacent-price pointers of the level chain.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct PriceLevel {
    pub head: Option<OrderId>,
    pub tail: Option<OrderId>,
    pub total_lots: Lots,
    pub order_count: u32,
    pub prev_price: Option<PriceTicks>,
    pub next_price: Option<PriceTicks>,
}

// ----------------------------------------------------------------------------
// Deterministic integer math
// ----------------------------------------------------------------------------

#[derive(
    Clone,
    Copy,
    Debug,
    Encode,
    Decode,
    DecodeWithMemTracking,
    Eq,
    PartialEq,
    TypeInfo,
    MaxEncodedLen,
)]
pub struct MarketPrecision {
    pub tick_size_quote: QuoteAmount,
    pub lot_size_base: u64,
    pub base_precision: u64,
}

fn to_u128(v: U256) -> Result<u128, MathError> {
    if v > U256::from(u128::MAX) {
        Err(MathError::Overflow)
    } else {
        Ok(v.as_u128())
    }
}

fn to_i128(v: U256) -> Result<i128, MathError> {
    let u = to_u128(v)?;
    i128::try_from(u).map_err(|_| MathError::Overflow)
}

/// `product(factors)` evaluated in `U256`.
fn product(factors: &[u128]) -> Result<U256, MathError> {
    let mut acc = U256::one();
    for f in factors {
        acc = acc.checked_mul(U256::from(*f)).ok_or(MathError::Overflow)?;
    }
    Ok(acc)
}

/// `floor(product(factors) / d)` evaluated in `U256`.
fn mul_div_floor(factors: &[u128], d: u128) -> Result<u128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }
    to_u128(product(factors)? / U256::from(d))
}

/// `ceil(product(factors) / d)` evaluated in `U256`.
fn mul_div_ceil(factors: &[u128], d: u128) -> Result<u128, MathError> {
    if d == 0 {
        return Err(MathError::DivisionByZero);
    }
    let divisor = U256::from(d);
    let acc = product(factors)?;
    to_u128((acc + divisor - U256::one()) / divisor)
}

impl MarketPrecision {
    /// `notional = price_ticks * tick_size_quote * lots * lot_size_base / base_precision`.
    pub fn notional(&self, price_ticks: PriceTicks, lots: Lots) -> Result<QuoteAmount, MathError> {
        mul_div_floor(
            &[
                price_ticks as u128,
                self.tick_size_quote,
                lots as u128,
                self.lot_size_base as u128,
            ],
            self.base_precision as u128,
        )
    }

    /// Signed PnL of holding `size_lots` across a price move of `diff_ticks` ticks.
    pub fn pnl(&self, size_lots: i64, diff_ticks: i128) -> Result<SignedQuote, MathError> {
        if size_lots == 0 || diff_ticks == 0 {
            return Ok(0);
        }
        let magnitude = mul_div_floor(
            &[
                size_lots.unsigned_abs() as u128,
                diff_ticks.unsigned_abs(),
                self.tick_size_quote,
                self.lot_size_base as u128,
            ],
            self.base_precision as u128,
        )?;
        let sign = (size_lots.signum() as i128) * (diff_ticks.signum());
        let signed = to_i128(U256::from(magnitude))?;
        sign.checked_mul(signed).ok_or(MathError::Overflow)
    }

    /// `ceil(amount * ppm / PPM_DENOM)`. Fees and margin requirements always round up.
    pub fn apply_ppm_ceil(&self, amount: QuoteAmount, ppm: u32) -> Result<QuoteAmount, MathError> {
        let acc = U256::from(amount)
            .checked_mul(U256::from(ppm))
            .ok_or(MathError::Overflow)?;
        let divisor = U256::from(PPM_DENOM);
        to_u128((acc + divisor - U256::one()) / divisor)
    }
}

/// `ceil(a * b / c)` for plain `u128` inputs, used for pro-rata reserve release.
pub fn mul_div_ceil_u128(a: u128, b: u128, c: u128) -> Result<u128, MathError> {
    if c == 0 {
        return Err(MathError::DivisionByZero);
    }
    let acc = U256::from(a)
        .checked_mul(U256::from(b))
        .ok_or(MathError::Overflow)?;
    let divisor = U256::from(c);
    to_u128((acc + divisor - U256::one()) / divisor)
}

/// Does a `taker` order on `side` with limit `limit` cross the resting `book_price`?
pub fn crosses(side: Side, limit: PriceTicks, book_price: PriceTicks) -> bool {
    match side {
        Side::Buy => limit >= book_price,
        Side::Sell => limit <= book_price,
    }
}

/// Signed lot delta contributed by an order on `side` filling `lots`.
pub fn signed_lots(side: Side, lots: Lots) -> i64 {
    match side {
        Side::Buy => lots as i64,
        Side::Sell => -(lots as i64),
    }
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
    fn notional_is_exact() {
        // 1 tick-lot == 1 USDC atom with the testnet parameters.
        assert_eq!(prec().notional(60_000, 1).unwrap(), 60_000);
        assert_eq!(prec().notional(60_000, 1_000_000).unwrap(), 60_000_000_000);
    }

    #[test]
    fn pnl_sign_is_correct() {
        assert_eq!(prec().pnl(10, 5).unwrap(), 50);
        assert_eq!(prec().pnl(-10, 5).unwrap(), -50);
        assert_eq!(prec().pnl(10, -5).unwrap(), -50);
        assert_eq!(prec().pnl(0, 5).unwrap(), 0);
    }

    #[test]
    fn ppm_rounds_up() {
        assert_eq!(prec().apply_ppm_ceil(1, 500_000).unwrap(), 1);
        assert_eq!(prec().apply_ppm_ceil(0, 500_000).unwrap(), 0);
        assert_eq!(prec().apply_ppm_ceil(999_999, 1).unwrap(), 1);
    }

    #[test]
    fn mul_div_ceil_rounds_up() {
        assert_eq!(mul_div_ceil_u128(10, 1, 3).unwrap(), 4);
        assert_eq!(mul_div_ceil_u128(10, 3, 3).unwrap(), 10);
        assert!(mul_div_ceil_u128(10, 1, 0).is_err());
    }
}
