//! # Sunchain Exchange Core (Phase 1)
//!
//! A single, performance critical FRAME pallet that carries the whole trading hot path:
//! market parameters, account collateral, margin checks, the price/time priority order book,
//! matching, positions, PnL and fees — all inside **one dispatch**.
//!
//! The code is layered internally (`book`, `risk`, `settlement`) but deliberately *not* split
//! into several pallets, so that no dispatch pays for repeated SCALE encoding, extra storage
//! round trips or intermediate state.
//!
//! Phase 1 scope (see `docs/Sunchain-Phase1-Exchange-Core-Detailed-Design.md`):
//! one collateral asset, one market, GTC / IOC / post-only / reduce-only, price-time priority
//! matching, initial margin checks, positions, realised and unrealised PnL and maker/taker fees.
//! No oracle, no funding, no liquidation, no bridge, no indexer.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use pallet::*;
pub use types::*;
pub use weights::WeightInfo;

pub mod book;
pub mod funding;
pub mod migrations;
pub mod risk;
pub mod settlement;
pub mod types;
pub mod weights;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;

use sp_runtime::{
	traits::ValidateUnsigned,
	transaction_validity::{
		InvalidTransaction, TransactionSource, TransactionValidity, ValidTransaction,
	},
};

#[frame_support::pallet]
pub mod pallet {
    use super::{funding, risk, settlement, types, weights::WeightInfo};
    use crate::types::{
    LiquidityTier,
        crosses, mul_div_ceil_u128, signed_lots, Account, AccountStatus, ActionClass, CancelReason,
        ClientOrderId, ExchangeAction, FeeTier, FeeTierId, Lots, Market, MarketConfig, MarketId,
        MarketStatus, MathError, Order, OrderId, OrderRequest, PartyState, Position, PriceLevel,
        PriceTicks, QuoteAmount, RiskParams, Side, SIGNED_ACTION_DOMAIN, SIGNED_ACTION_VERSION,
        SignedExchangeAction, SignedQuote, SortKey, TimeInForce, PPM_DENOM,
    };
    use alloc::vec::Vec;
    use codec::{Decode, Encode, MaxEncodedLen};
    use exchange_primitives::{
        AdlRank, ExchangeEngine, InsuranceFund, PriceOracle, ResidualLoss, SubaccountId,
        DEFAULT_SUBACCOUNT,
    };
    use frame_support::{
        dispatch::PostDispatchInfo,
        pallet_prelude::*,
        storage::TransactionOutcome,
        traits::{
            tokens::{
                fungibles::{Inspect, Mutate},
                Preservation,
            },
            EnsureOrigin,
        },
        PalletId,
    };
    use frame_system::pallet_prelude::*;
    use scale_info::TypeInfo;
    use sp_runtime::{
        traits::{
            AccountIdConversion, AtLeast32BitUnsigned, CheckedAdd, CheckedSub, One,
            SaturatedConversion, Saturating, Zero,
        },
        DispatchError,
    };

    /// The current storage version.
    ///
    /// Phase 1 never declared one, so any chain carrying Phase 1 state is at version `0`.
    /// The P2-B layout (`Position::funding_index`, settled `collateral`, `BadDebt`) is `1`;
    /// see [`migrations`].
    const STORAGE_VERSION: StorageVersion = StorageVersion::new(3);

    #[pallet::pallet]
    #[pallet::storage_version(STORAGE_VERSION)]
    pub struct Pallet<T>(_);

    // ------------------------------------------------------------------ config

    #[pallet::config]
    pub trait Config: frame_system::Config {
        /// The overarching runtime event type.
        type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

        /// Fungible asset adapter used for the collateral token. Phase 1 wires this to
        /// `pallet_assets`; the pallet never couples to a concrete asset pallet.
        type Assets: Inspect<Self::AccountId, AssetId = Self::AssetId, Balance = Self::Balance>
            + Mutate<Self::AccountId>;

        /// Asset identifier type of the collateral token.
        type AssetId: Parameter + Member + Copy + MaxEncodedLen + TypeInfo;

        /// Balance type of the collateral token. Must be convertible to/from the internal
        /// `u128` quote representation.
        type Balance: Parameter
            + Member
            + Copy
            + MaxEncodedLen
            + TypeInfo
            + AtLeast32BitUnsigned
            + CheckedAdd
            + CheckedSub
            + TryFrom<QuoteAmount>
            + TryInto<QuoteAmount>;

        /// Origin allowed to create markets and change risk parameters.
        type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

        /// Deterministic custody account for user collateral.
        type PalletId: Get<PalletId>;

        /// The single collateral asset accepted by the core.
        type CollateralAssetId: Get<Self::AssetId>;

        type MaxMarkets: Get<u32>;
        /// Upper bound on sub-accounts per account (§9.1).
        type MaxSubaccountsPerAccount: Get<u32>;
        /// Upper bound on markets read by one cross-market risk aggregation (§9.2). Exceeding it
        /// refuses new market positions rather than silently under-aggregating risk.
        type MaxClearingMarkets: Get<u32>;
        type MaxOpenOrdersPerAccount: Get<u32>;
        type MaxPriceLevelsPerSide: Get<u32>;
        type MaxOrdersPerLevel: Get<u32>;
        type MaxMatchesPerAction: Get<u32>;
        type MaxActionsPerBatch: Get<u32>;

        /// Signature authorising a single action inside a proposer-built batch (§12.4).
        ///
        /// This is what makes an exchange batch safe: each action carries its own
        /// authorisation, so the account that submits the batch cannot act for anyone else.
        type Signature: sp_runtime::traits::Verify<Signer = Self::Signer>
            + Parameter
            + MaxEncodedLen;

        /// Public key behind [`Config::Signature`]; resolves to the authorising account.
        type Signer: sp_runtime::traits::IdentifyAccount<AccountId = Self::AccountId>
            + Parameter
            + MaxEncodedLen;
        /// Upper bound on orders that one `prune_orders` call may remove.
        type MaxPrunePerCall: Get<u32>;
        type MaxOrderLifetime: Get<BlockNumberFor<Self>>;

        // ---- liquidation (Phase 2 P2-B, §8.2 / §8.3) --------------------------
        /// Safety buffer added to the maintenance requirement when sizing a partial
        /// liquidation (§8.2 `B`).
        type MaintenanceBufferPpm: Get<u32>;
        /// Penalty charged on the notional a liquidation closes (§8.3 step 6).
        type LiquidationPenaltyPpm: Get<u32>;
        /// Share of that penalty paid to the keeper who submitted the liquidation.
        type LiquidationKeeperRebatePpm: Get<u32>;
        /// Worst price, relative to the mark, a liquidation may fill at (§8.3 step 4).
        type MaxLiquidationSlippagePpm: Get<u32>;
        /// Lower bound on the lots one partial liquidation closes (§8.2).
        type MinLiquidationLots: Get<Lots>;

        // ---- funding (Phase 2 P2-A, dYdX two-level epochs) --------------------
        /// `funding-sample` epoch length in blocks. Each epoch produces one premium sample.
        type FundingSamplePeriod: Get<BlockNumberFor<Self>>;
        /// `funding-tick` epoch length in blocks. Each tick consumes the samples and moves
        /// the funding index.
        type FundingTickPeriod: Get<BlockNumberFor<Self>>;
        /// Blocks over which a full funding rate is charged (dYdX: 8 hours).
        type FundingRatePeriod: Get<BlockNumberFor<Self>>;
        type MaxPremiumSamples: Get<u32>;
        /// Premium votes a proposer may submit for one sample epoch (P2-D).
        type MaxPremiumVotes: Get<u32>;
        /// Share removed from both ends of the sample list before averaging (dYdX
        /// `RemovedTailSampleRatioPpm`).
        type RemovedTailSampleRatioPpm: Get<u32>;
        /// Added to the premium; a constant bias (dYdX `DefaultFundingPpm`).
        type DefaultFundingPpm: Get<i32>;
        /// `|rate| <= factor * (initial margin - maintenance margin)`.
        type FundingRateClampFactorPpm: Get<u32>;
        type MaxAbsPremiumPpm: Get<u32>;
        /// Notional used to compute the impact bid / ask price (dYdX
        /// `ImpactNotionalQuoteQuantums`).
        type ImpactNotionalQuote: Get<QuoteAmount>;

        /// Maximum number of `ExchangeCore` user actions accepted in one block (design §11.2).
        /// Price source (Phase 2 P2-A). `NoOracle` until `pallet-oracle` is registered; the
        /// core then falls back to the Root-set cached testnet price.
        type PriceOracle: PriceOracle<MarketId>;

        /// Backstop that absorbs losses an account can no longer pay (§8.4). It is spent
        /// **before** any loss is socialised through ADL; when it is exhausted the remainder
        /// is booked to `BadDebt`.
        type Insurance: InsuranceFund<QuoteAmount>;

        type MaxExchangeActionsPerBlock: Get<u32>;
        /// Weight budget reserved for `ExchangeCore` user actions in one block (design §11.2).
        /// Phase 1 has no dedicated exchange transaction pool, so without this cap the generic
        /// pool could fill a whole block with order flow and starve staking / governance / EVM.
        type ExchangeBlockWeightBudget: Get<Weight>;

        type WeightInfo: WeightInfo;
    }

    // ------------------------------------------------------------------ storage

    #[pallet::storage]
    pub type NextMarketId<T> = StorageValue<_, MarketId, ValueQuery>;

    /// Phase 1 has exactly one market; this points at it so risk checks that need a mark price
    /// stay O(1). Set the first time a market is created.
    #[pallet::storage]
    pub type PrimaryMarketId<T> = StorageValue<_, MarketId, OptionQuery>;

    #[pallet::storage]
    pub type Markets<T> =
        StorageMap<_, Blake2_128Concat, MarketId, Market<BlockNumberFor<T>>, OptionQuery>;

    #[pallet::storage]
    /// Keyed by `(AccountId, SubaccountId)` per §9.1.
    pub type Accounts<T> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        Blake2_128Concat,
        SubaccountId,
        Account,
        OptionQuery,
    >;

    /// Bounded list of an account's sub-accounts, so they can be enumerated without an
    /// unbounded prefix iterator (§9.1).
    #[pallet::storage]
    pub type SubaccountIndex<T> = StorageMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        BoundedVec<SubaccountId, <T as Config>::MaxSubaccountsPerAccount>,
        ValueQuery,
    >;

    /// Keyed by `(AccountId, SubaccountId, MarketId)` per §9.1.
    #[pallet::storage]
    pub type Positions<T> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, <T as frame_system::Config>::AccountId>,
            NMapKey<Blake2_128Concat, SubaccountId>,
            NMapKey<Blake2_128Concat, MarketId>,
        ),
        Position,
        OptionQuery,
    >;

    #[pallet::storage]
    pub type Orders<T> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        MarketId,
        Blake2_128Concat,
        OrderId,
        Order<<T as frame_system::Config>::AccountId, BlockNumberFor<T>>,
        OptionQuery,
    >;

    #[pallet::storage]
    pub type PriceLevels<T> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, MarketId>,
            NMapKey<Blake2_128Concat, Side>,
            NMapKey<Blake2_128Concat, PriceTicks>,
        ),
        PriceLevel,
        OptionQuery,
    >;

    /// O(1) lookup of a resting order by `(owner, client_order_id)`.
    #[pallet::storage]
    pub type UserOrderByClientId<T> = StorageNMap<
        _,
        (
            NMapKey<Blake2_128Concat, <T as frame_system::Config>::AccountId>,
            NMapKey<Blake2_128Concat, SubaccountId>,
            NMapKey<Blake2_128Concat, ClientOrderId>,
        ),
        (MarketId, OrderId),
        OptionQuery,
    >;

    /// Fee tiers available to accounts (§9.3).
    #[pallet::storage]
    pub type FeeTiers<T> = StorageMap<_, Blake2_128Concat, FeeTierId, FeeTier, OptionQuery>;

    /// Last consumed `action_nonce` per `(AccountId, SubaccountId)` (§12.3).
    ///
    /// Replay protection for pre-signed actions: a batch action may only run once, and
    /// actions from one sub-account must run in nonce order.
    #[pallet::storage]
    pub type ActionNonce<T> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        Blake2_128Concat,
        SubaccountId,
        u64,
        ValueQuery,
    >;

    /// Tier assigned to a sub-account. `None` means "use the market's own fee schedule".
    #[pallet::storage]
    pub type AccountFeeTier<T> = StorageDoubleMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        Blake2_128Concat,
        SubaccountId,
        FeeTierId,
        OptionQuery,
    >;

    /// Collected trading fees, still held by the custody account.
    #[pallet::storage]
    pub type FeePool<T> = StorageValue<_, QuoteAmount, ValueQuery>;

    /// Losses that could not be collected because the account ran out of collateral.
    ///
    /// Funding is zero-sum: the winning side is credited whether or not the loser can pay.
    /// Dropping the uncollectable part would break the custody conservation identity
    /// (§4.3), so it is booked here. This is a stop-gap for P2-B step 1; `pallet-insurance`
    /// (step 2) must take it over and clear it.
    #[pallet::storage]
    pub type BadDebt<T> = StorageValue<_, QuoteAmount, ValueQuery>;

    /// The same uncollectable loss, attributed per account (§8.5.1 §2).
    ///
    /// `BadDebt` is the global figure the conservation identity needs; this map is the audit
    /// trail — it says *whose* shortfall it was and which side of the market it came from, which
    /// is what ADL candidate selection keys off.
    #[pallet::storage]
    pub type ResidualLossOf<T> = StorageMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        ResidualLoss,
        ValueQuery,
    >;

    /// Rebate accrued by past liquidations but not yet claimed by a keeper.
    ///
    /// The penalty is charged to the liquidated account and split here: the insurance share is
    /// transferred immediately, the keeper share waits until the keeper asks for it. Keeping it
    /// per keeper (rather than paying inline) means a keeper can never be
    /// paid out of user collateral — nor out of another keeper's pending rebate.
    #[pallet::storage]
    pub type KeeperRebateOf<T> = StorageMap<
        _,
        Blake2_128Concat,
        <T as frame_system::Config>::AccountId,
        QuoteAmount,
        ValueQuery,
    >;

    // ------------------------------------------------------------------ genesis

    /// A brand new chain has no markets and no way to make one until governance acts, so the
    /// default set is configured here. dYdX lists markets through governance proposals; we
    /// need both, and this is the "chain starts usable" half.
    #[pallet::genesis_config]
    #[derive(frame_support::DefaultNoBound)]
    pub struct GenesisConfig<T: Config> {
        /// Risk tiers, in id order starting at 0.
        pub tiers: Vec<LiquidityTier>,
        /// Markets to create, assigned ids in order.
        pub markets: Vec<MarketConfig>,
        #[serde(skip)]
        pub _unused: core::marker::PhantomData<T>,
    }

    #[pallet::genesis_build]
    impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
        fn build(&self) {
            for (id, tier) in self.tiers.iter().enumerate() {
                LiquidityTiers::<T>::insert(id as u32, *tier);
            }
            for config in self.markets.iter() {
                // A broken market in the spec should stop genesis, not produce a chain that
                // silently lacks the market everyone assumes is there.
                Pallet::<T>::do_create_market(config.clone()).expect("genesis market is valid");
            }
        }
    }

    /// Bounded, and cleared at every tick, so it can never grow without limit.
    #[pallet::storage]
    pub type PremiumSamples<T> = StorageMap<
        _,
        Blake2_128Concat,
        MarketId,
        BoundedVec<i32, <T as Config>::MaxPremiumSamples>,
        ValueQuery,
    >;

    /// Risk tiers, shared by markets (dYdX v4 `LiquidityTier`).
    #[pallet::storage]
    pub type LiquidityTiers<T: Config> =
        StorageMap<_, Blake2_128Concat, u32, LiquidityTier, OptionQuery>;

    /// Premium votes submitted for the sample epoch in progress, per market (P2-D).
    ///
    /// dYdX has each proposer vote the premium every block and takes the median at the
    /// epoch boundary. The point is to get several observations per epoch without walking
    /// the book on-chain every block — far too expensive — which is why the votes come
    /// from outside.
    #[pallet::storage]
    pub type PremiumVotes<T> = StorageMap<
        _,
        Twox64Concat,
        MarketId,
        BoundedVec<i32, <T as Config>::MaxPremiumVotes>,
        ValueQuery,
    >;

    /// Last premium-vote nonce accepted for each market.
    ///
    /// A vote carries nothing else that changes between blocks: the same premium encodes to
    /// the same extrinsic, which is the same hash. The pool bans the hash of anything it
    /// finds inside an imported block, so without a nonce the *first* successful vote
    /// permanently bans itself — and every later vote for that market then comes back
    /// `TemporarilyBanned`, for as long as the premium does not change. The feed votes every
    /// block, so that is forever, and the only symptom is a flood of log lines.
    #[pallet::storage]
    pub type PremiumVoteNonce<T> = StorageMap<_, Twox64Concat, MarketId, u64, ValueQuery>;

    /// `ExchangeCore` usage of the current block: `(block, actions, declared_weight)`.
    ///
    /// Reset lazily on the first action of a new block, so no `on_initialize` hook (and
    /// therefore no per-block storage write) is needed.
    #[pallet::storage]
    pub type BlockExchangeUsage<T> = StorageValue<_, (BlockNumberFor<T>, u32, Weight), OptionQuery>;

    // ------------------------------------------------------------------ events

    #[pallet::event]
    #[pallet::generate_deposit(pub(crate) fn deposit_event)]
    pub enum Event<T: Config> {
		/// A risk tier changed; `markets_updated` is how many inherited it.
		LiquidityTierUpdated { tier_id: u32, markets_updated: u32 },
        CollateralDeposited {
            who: T::AccountId,
            amount: QuoteAmount,
        },
        CollateralWithdrawn {
            who: T::AccountId,
            amount: QuoteAmount,
        },
        MarketCreated {
            market_id: MarketId,
        },
        MarketStatusChanged {
            market_id: MarketId,
            status: MarketStatus,
        },
        MarkPriceUpdated {
            market_id: MarketId,
            price_ticks: PriceTicks,
            at: BlockNumberFor<T>,
        },
        MarketsRiskParamsUpdated {
            market_id: MarketId,
        },
        /// A fee tier was created or replaced (§9.3).
        FeeTierSet {
            id: FeeTierId,
        },
        /// A sub-account's fee tier changed (§9.3).
        AccountFeeTierSet {
            who: T::AccountId,
            sub: SubaccountId,
            tier: Option<FeeTierId>,
        },
        /// A liquidatable account was force-closed (§8.3).
        PositionLiquidated {
            who: T::AccountId,
            market_id: MarketId,
            lots: Lots,
            penalty: QuoteAmount,
            insurance_delta: QuoteAmount,
        },
        /// A bankrupt position was offset through ADL (§8.5).
        AdlClosed {
            who: T::AccountId,
            market_id: MarketId,
            lots: Lots,
            price_ticks: PriceTicks,
        },
        /// One action of an exchange batch failed; the batch continued (§12.4).
        ExchangeActionFailed {
            who: T::AccountId,
            subaccount: SubaccountId,
            action_nonce: u64,
            error: DispatchError,
        },
        /// A loss survived collateral, liquidation and the insurance fund (§8.5.1 §2).
        ResidualLossRecorded {
            who: T::AccountId,
            market_id: MarketId,
            amount: QuoteAmount,
        },
        OrderAccepted {
            who: T::AccountId,
            client_order_id: ClientOrderId,
            market_id: MarketId,
            order_id: OrderId,
        },
        TradeExecuted {
            market_id: MarketId,
            maker: T::AccountId,
            taker: T::AccountId,
            price_ticks: PriceTicks,
            lots: Lots,
            maker_order_id: OrderId,
            taker_client_order_id: ClientOrderId,
        },
        OrderCancelled {
            who: T::AccountId,
            client_order_id: ClientOrderId,
            reason: CancelReason,
            released_margin: QuoteAmount,
        },
        OrderFilled {
            who: T::AccountId,
            client_order_id: ClientOrderId,
        },
        AccountRestricted {
            who: T::AccountId,
            equity: SignedQuote,
            maintenance_margin: QuoteAmount,
        },
        AccountUnrestricted {
            who: T::AccountId,
            equity: SignedQuote,
        },
        MatchLimitReached {
            who: T::AccountId,
            client_order_id: ClientOrderId,
            remaining_lots: Lots,
        },
        OrdersInvalidatedUpTo {
            who: T::AccountId,
            client_order_id: ClientOrderId,
        },
        // ---- funding (P2-A) ----
        /// One premium observation recorded for the current `funding-tick` epoch.
        FundingSampleRecorded {
            market_id: MarketId,
            premium_ppm: i32,
        },
        /// A `funding-tick` epoch moved the funding index.
        FundingSettled {
            market_id: MarketId,
            rate_ppm: i32,
            funding_index: SignedQuote,
            net_oi_lots: i64,
        },
    }

    // ------------------------------------------------------------------ errors

    #[pallet::error]
    pub enum Error<T> {
		/// A market referenced a risk tier that does not exist.
		UnknownLiquidityTier,
        MarketNotFound,
        MarketNotActive,
        MarketReduceOnly,
        MarketCancelOnly,
        MaxMarketsReached,
        InvalidPrice,
        InvalidQuantity,
        InvalidAmount,
        InvalidExpiry,
        InvalidMarketConfig,
        DuplicateClientOrderId,
        ClientOrderIdInvalidated,
        PostOnlyWouldCross,
        PostOnlyRequiresGtc,
        ReduceOnlyWouldIncrease,
        InsufficientCollateral,
        MarkPriceStale,
        /// The account is not below its maintenance requirement, so it may not be liquidated.
        NotLiquidatable,
        /// The book could not absorb the liquidation inside the allowed slippage band.
        LiquidationSlippageExceeded,
        /// The account already holds positions in `MaxClearingMarkets` other markets (§9.2).
        ///
        /// Refusing is deliberate: aggregating only the first N markets would *underestimate*
        /// the requirement, which is worse than simply not allowing another market.
        TooManyClearingMarkets,
        /// A maker rebate was due but `FeePool` cannot cover it (§9.3). The rebate is paid out
        /// of collected fees, so an empty pool must be topped up by the insurance fund or
        /// governance rather than silently diluting user collateral.
        InsufficientFeePool,
        /// The referenced fee tier does not exist.
        UnknownFeeTier,
        AccountRestricted,
        OrderNotFound,
        NotOrderOwner,
        OrderAlreadyInvalidated,
        TooManyOpenOrders,
        TooManyPriceLevels,
        TooManyOrdersAtLevel,
        TooManyMatches,
        TooManyActions,
        ArithmeticOverflow,
        AssetTransferFailed,
        InvariantViolation,
        /// The per-block budget reserved for exchange actions is exhausted (design §11.2).
        ExchangeBlockBudgetExceeded,
        /// The signature on a batch action does not match its claimed signer.
        InvalidActionSignature,
        /// `action_nonce` must be greater than the last one consumed (never reused).
        InvalidActionNonce,
        /// The signature has expired: `expires_at` is at or before the current block.
        ///
        /// Checked *before* the nonce is consumed, so an expired action leaves the account
        /// exactly where it was and the same nonce can be re-signed.
        ActionExpired,
        /// The signature's `expires_at` is further out than `MaxOrderLifetime`.
        InvalidActionExpiry,
        /// Sub-accounts are not supported yet.
        ///
        /// The nonce is keyed by `(account, subaccount)`, but orders, positions and
        /// collateral are not: every trading path reads and writes `DEFAULT_SUBACCOUNT`
        /// (`do_place_order`, `do_cancel_order`, `account_of`, `position_of`). A non-zero
        /// sub-account would therefore be authorised against one key and applied to
        /// another — `CancelUpTo` even reads the default account and writes the signed
        /// one. Refusing is the only honest answer until the whole state machine carries
        /// `subaccount` (see `docs/Sunchain-DEX-Remediation-Plan.md`, S0-1 / S3-1).
        SubaccountUnsupported,
    }


	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			let Call::execute_exchange_batch { actions } = call else {
				return Self::validate_premium_votes(call)
			};
			// Built locally by whoever proposes the block; never relay them.
			if matches!(source, TransactionSource::External) {
				return InvalidTransaction::Call.into()
			}
			if actions.is_empty() || actions.len() as u32 > T::MaxActionsPerBatch::get() {
				return InvalidTransaction::Call.into()
			}

			// The batch must already be in canonical order (S1-3).
			//
			// The key is (class, account, nonce, action hash): signed, or derived from signed
			// bytes, so a proposer cannot choose it. An out-of-order batch therefore means the
			// proposer chose an order — the exact thing this check exists to prevent. Rejecting
			// with `Call` rather than `Stale`: re-submitting the same batch would fail the same
			// way, so there is nothing to wait for.
			let mut previous: Option<SortKey> = None;
			for action in actions.iter() {
				let key = action.sort_key();
				if previous.is_some_and(|prev| prev >= key) {
					return InvalidTransaction::Call.into()
				}
				previous = Some(key);
			}

			// Reject a batch that provably cannot do anything — every action already
			// consumed or badly signed. Otherwise the pool could fill with unsigned entries
			// that are guaranteed no-ops. Advisory only: the replay inside the call
			// remains the sole authority.
			let mut builder = ValidTransaction::with_tag_prefix("ExchangeBatch")
				.priority(T::MaxActionsPerBatch::get().into())
				// A batch is only ever valid for the block it was built for.
				.longevity(10)
				.propagate(false);
			let mut usable = 0u32;
			for action in actions.iter() {
				if Self::validate_signed_action(action).is_ok() {
					usable = usable.saturating_add(1);
					// Dedupe on action identity, so a replay cannot occupy two slots.
					builder = builder.and_provides((action.signer.encode(), action.action_nonce));
				}
			}
			if usable == 0 {
				return InvalidTransaction::Stale.into()
			}
			builder.build()
		}
	}

    // ------------------------------------------------------------------ hooks

    #[pallet::hooks]
    impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
        /// Drives the two funding epochs.
        ///
        /// Iteration is bounded by `MaxMarkets`: `on_initialize` must never scan unbounded
        /// storage (design §16.2).
        fn on_initialize(now: BlockNumberFor<T>) -> Weight {
            let sample_period = T::FundingSamplePeriod::get();
            let tick_period = T::FundingTickPeriod::get();
            let is_tick = !tick_period.is_zero() && (now % tick_period).is_zero();
            let is_sample = !sample_period.is_zero() && (now % sample_period).is_zero();
            if !is_tick && !is_sample {
                return Weight::zero();
            }

            let max_markets = T::MaxMarkets::get();
            let mut processed: u32 = 0;
            // `Markets` is a map; bound the number of entries touched per block.
            for market_id in Markets::<T>::iter_keys() {
                if processed >= max_markets {
                    break;
                }
                processed = processed.saturating_add(1);
                let Some(mut market) = Markets::<T>::get(market_id) else {
                    continue;
                };
                // Tick first: it consumes the samples of the epoch that just ended, then the
                // new sample starts the next cycle (same ordering as dYdX).
                if is_tick {
                    Self::process_funding_tick(market_id, &mut market, now);
                }
                if is_sample {
                    Self::process_funding_sample(market_id, &market, now);
                }
                Markets::<T>::insert(market_id, market);
            }
            let _ = processed;
            T::WeightInfo::on_initialize()
        }
    }

    // ------------------------------------------------------------------ calls

    #[pallet::call]
    impl<T: Config> Pallet<T> {
        /// Moves test USDC into the exchange custody account and credits the internal ledger.
        #[pallet::call_index(0)]
        #[pallet::weight(T::WeightInfo::deposit_collateral())]
        pub fn deposit_collateral(origin: OriginFor<T>, amount: QuoteAmount) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(amount > 0, Error::<T>::InvalidAmount);
            Self::consume_block_budget(T::WeightInfo::deposit_collateral())?;

            let asset = T::CollateralAssetId::get();
            let balance = Self::to_balance(amount)?;
            let custody = Self::custody_account();
            T::Assets::transfer(asset, &who, &custody, balance, Preservation::Expendable)
                .map_err(|_| Error::<T>::AssetTransferFailed)?;

            Accounts::<T>::try_mutate(&who, DEFAULT_SUBACCOUNT, |maybe| -> DispatchResult {
                let account = maybe.get_or_insert_with(Account::default);
                account.collateral = account
                    .collateral
                    .checked_add(amount)
                    .ok_or(Error::<T>::ArithmeticOverflow)?;
                Ok(())
            })?;

            Self::deposit_event(Event::CollateralDeposited { who, amount });
            Ok(())
        }

        /// Withdraws free collateral after re-checking the initial margin requirement.
        #[pallet::call_index(1)]
        #[pallet::weight(T::WeightInfo::withdraw_collateral())]
        pub fn withdraw_collateral(origin: OriginFor<T>, amount: QuoteAmount) -> DispatchResult {
            let who = ensure_signed(origin)?;
            ensure!(amount > 0, Error::<T>::InvalidAmount);
            Self::consume_block_budget(T::WeightInfo::withdraw_collateral())?;

            let mut account = Self::account_of(&who);
            ensure!(
                !matches!(account.status, AccountStatus::Restricted),
                Error::<T>::AccountRestricted
            );

            // Settle before the balance check (P2-B §7): otherwise an account could dodge
            // accrued funding, or withdraw cash its realised losses have already consumed.
            if let Some(market_id) = PrimaryMarketId::<T>::get() {
                if let Some(market) = Markets::<T>::get(market_id) {
                    let position = Self::position_of(&who, market_id);
                    // Only touch storage when there is actually something to settle; otherwise
                    // every withdrawal would create a position entry for non-traders.
                    let dirty = !position.is_flat()
                        || position.realized_pnl != 0
                        || position.funding_index != market.funding_index;
                    if dirty {
                        let mut position = position;
                        Self::settle_party(
                            &who,
                            market_id,
                            &mut account,
                            &mut position,
                            market.funding_index,
                        )?;
                        Positions::<T>::insert((&who, DEFAULT_SUBACCOUNT, market_id), position);
                    }
                }
            }

            ensure!(
                account.collateral >= amount,
                Error::<T>::InsufficientCollateral
            );

            // Only accounts that actually carry risk need a fresh mark price.
            if account.reserved_open_order_margin > 0 {
                let market_id = PrimaryMarketId::<T>::get().ok_or(Error::<T>::MarketNotFound)?;
                let market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
                let mark = Self::mark_price_of(market_id, &market)?;
                let position = Self::position_of(&who, market_id);
                let params = Self::params_of(&market, mark);
                let mut after = account.clone();
                after.collateral = after
                    .collateral
                    .checked_sub(amount)
                    .ok_or(Error::<T>::InsufficientCollateral)?;
                let view = risk::RiskView::of(&after, &position, params);
                ensure!(
                    Self::math(view.passes_initial_margin())?,
                    Error::<T>::InsufficientCollateral
                );
            } else if let Some(market_id) = PrimaryMarketId::<T>::get() {
                let position = Self::position_of(&who, market_id);
                if !position.is_flat() {
                    let market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
                    let mark = Self::mark_price_of(market_id, &market)?;
                    let params = Self::params_of(&market, mark);
                    let mut after = account.clone();
                    after.collateral = after
                        .collateral
                        .checked_sub(amount)
                        .ok_or(Error::<T>::InsufficientCollateral)?;
                    let view = risk::RiskView::of(&after, &position, params);
                    ensure!(
                        Self::math(view.passes_initial_margin())?,
                        Error::<T>::InsufficientCollateral
                    );
                }
            }

            let asset = T::CollateralAssetId::get();
            let balance = Self::to_balance(amount)?;
            let custody = Self::custody_account();
            T::Assets::transfer(asset, &custody, &who, balance, Preservation::Expendable)
                .map_err(|_| Error::<T>::AssetTransferFailed)?;

            account.collateral = account
                .collateral
                .checked_sub(amount)
                .ok_or(Error::<T>::InsufficientCollateral)?;
            Accounts::<T>::insert(&who, DEFAULT_SUBACCOUNT, account);

            Self::deposit_event(Event::CollateralWithdrawn { who, amount });
            Ok(())
        }

        /// Places an order, matches it against the book and, for GTC, rests the remainder.
        #[pallet::call_index(2)]
        #[pallet::weight(T::WeightInfo::place_order(
            T::MaxMatchesPerAction::get(),
            T::MaxMatchesPerAction::get(),
            T::MaxPriceLevelsPerSide::get(),
        ))]
        pub fn place_order(
            origin: OriginFor<T>,
            request: OrderRequest<BlockNumberFor<T>>,
        ) -> DispatchResultWithPostInfo {
            let who = ensure_signed(origin)?;
            let outcome = Self::do_place_order(&who, request)?;
            let actual = T::WeightInfo::place_order(
                outcome.matches,
                outcome.stale_removed,
                outcome.level_scans,
            );
            if let Err(e) = Self::consume_block_budget(actual) {
            	sp_runtime::print(alloc::format!("exchange: batch dispatch: block budget refused: {:?}", e).as_str());
            	return Err(e.into())
            }
            Ok(PostDispatchInfo {
                actual_weight: Some(actual),
                pays_fee: Pays::Yes,
            })
        }

        /// Cancels one resting order in O(1).
        #[pallet::call_index(3)]
        #[pallet::weight(T::WeightInfo::cancel_order())]
        pub fn cancel_order(
            origin: OriginFor<T>,
            client_order_id: ClientOrderId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::consume_block_budget(T::WeightInfo::cancel_order())?;
            Self::do_cancel_order(&who, client_order_id)
        }

        /// Logically cancels every resting order with `client_order_id < n` in O(1).
        #[pallet::call_index(4)]
        #[pallet::weight(T::WeightInfo::cancel_up_to())]
        pub fn cancel_up_to(
            origin: OriginFor<T>,
            client_order_id: ClientOrderId,
        ) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::consume_block_budget(T::WeightInfo::cancel_up_to())?;
            let mut account = Self::account_of(&who);
            if client_order_id > account.invalid_before {
                account.invalid_before = client_order_id;
                Accounts::<T>::insert(&who, DEFAULT_SUBACCOUNT, account);
            }
            Self::deposit_event(Event::OrdersInvalidatedUpTo {
                who,
                client_order_id,
            });
            Ok(())
        }

        /// Executes a bounded, atomic batch of the caller's own actions.
        #[pallet::call_index(5)]
        #[pallet::weight(T::WeightInfo::execute_batch(
			T::MaxActionsPerBatch::get(),
			T::MaxActionsPerBatch::get().saturating_mul(T::MaxMatchesPerAction::get()),
			T::MaxActionsPerBatch::get().saturating_mul(T::MaxMatchesPerAction::get()),
		))]
        pub fn execute_batch(
            origin: OriginFor<T>,
            actions: BoundedVec<ExchangeAction<BlockNumberFor<T>>, T::MaxActionsPerBatch>,
        ) -> DispatchResultWithPostInfo {
            let who = ensure_signed(origin)?;
            ensure!(!actions.is_empty(), Error::<T>::TooManyActions);
            ensure!(
                actions.len() as u32 <= T::MaxActionsPerBatch::get(),
                Error::<T>::TooManyActions
            );

            let mut total_matches: u32 = 0;
            let mut total_stale: u32 = 0;
            let mut total_scans: u32 = 0;
            for action in actions.iter() {
                match action {
                    ExchangeAction::Place(request) => {
                        let outcome = Self::do_place_order(&who, request.clone())?;
                        total_matches = total_matches.saturating_add(outcome.matches);
                        total_stale = total_stale.saturating_add(outcome.stale_removed);
                        total_scans = total_scans.saturating_add(outcome.level_scans);
                    }
                    ExchangeAction::Cancel(client_order_id) => {
                        Self::do_cancel_order(&who, *client_order_id)?;
                    }
                    ExchangeAction::CancelUpTo(client_order_id) => {
                        let mut account = Self::account_of(&who);
                        if *client_order_id > account.invalid_before {
                            account.invalid_before = *client_order_id;
                            Accounts::<T>::insert(&who, DEFAULT_SUBACCOUNT, account);
                        }
                        Self::deposit_event(Event::OrdersInvalidatedUpTo {
                            who: who.clone(),
                            client_order_id: *client_order_id,
                        });
                    }
                }
            }

            let actual =
                T::WeightInfo::execute_batch(actions.len() as u32, total_matches, total_stale);
            Self::consume_block_budget(actual)?;
            Ok(PostDispatchInfo {
                actual_weight: Some(actual),
                pays_fee: Pays::Yes,
            })
        }

        /// Executes a proposer-built batch of *other* users' pre-signed actions (§12.4).
        ///
        /// Every action is authorised by its own signature, never by the account that submitted
        /// the batch. Actions run in the deterministic order of §12.3, so the submitter cannot
        /// decide who is filled first. A matching action that fails is skipped with an event
        /// rather than aborting the batch — otherwise one user could censor everyone else in the
        /// block. (Fund actions are not in this enum yet; when added they must abort the batch.)
        /// Submits this block's premium observations for a market (P2-D).
        ///
        /// Built by the proposer, so it is unsigned — same reasoning as
        /// [`Call::execute_exchange_batch`]. Votes accumulate until the sample epoch ends,
        /// at which point the boundary takes their median.
        /// Adds or updates a risk tier, and refreshes every market that uses it.
        ///
        /// The refresh is the point of having tiers at all: one change, all the markets on
        /// that tier. It is also why this is dangerous — it moves the margin of every open
        /// position on those markets at once, which is exactly what the governance design
        /// note (worklog 2026-09-18) wants a separate, slower process for.
        #[pallet::call_index(16)]
        #[pallet::weight(T::WeightInfo::create_market())]
        pub fn set_liquidity_tier(
            origin: OriginFor<T>,
            tier_id: u32,
            tier: LiquidityTier,
        ) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;
            ensure!(
                tier.maintenance_margin_ppm > 0
                    && tier.maintenance_margin_ppm <= tier.initial_margin_ppm
                    && tier.initial_margin_ppm as u128 <= PPM_DENOM,
                Error::<T>::InvalidMarketConfig
            );
            LiquidityTiers::<T>::insert(tier_id, tier);

            let mut updated = 0u32;
            for market_id in Markets::<T>::iter_keys() {
                let Some(mut market) = Markets::<T>::get(market_id) else { continue };
                if market.liquidity_tier != tier_id {
                    continue
                }
                market.initial_margin_ppm = tier.initial_margin_ppm;
                market.maintenance_margin_ppm = tier.maintenance_margin_ppm;
                Markets::<T>::insert(market_id, market);
                updated = updated.saturating_add(1);
            }
            Self::deposit_event(Event::LiquidityTierUpdated { tier_id, markets_updated: updated });
            Ok(())
        }

        #[pallet::call_index(15)]
        #[pallet::weight(T::WeightInfo::cancel_order())]
        pub fn submit_premium_votes(
            origin: OriginFor<T>,
            market_id: MarketId,
            votes: BoundedVec<i32, T::MaxPremiumVotes>,
            nonce: u64,
        ) -> DispatchResult {
            ensure_none(origin)?;
            ensure!(!votes.is_empty(), Error::<T>::TooManyActions);
            // Taken before the votes are queued, so a replay cannot queue a second time —
            // which is the whole reason the nonce exists.
            ensure!(
                nonce > PremiumVoteNonce::<T>::get(market_id),
                Error::<T>::InvalidActionNonce
            );
            PremiumVoteNonce::<T>::insert(market_id, nonce);
            PremiumVotes::<T>::mutate(market_id, |queued| {
                for vote in votes.iter() {
                    // Bounded: a full queue simply stops accumulating. The epoch falls back
                    // to fewer votes; it is never a reason to fail the extrinsic.
                    let _ = queued.try_push(*vote);
                }
            });
            Ok(())
        }

        #[pallet::call_index(14)]
        #[pallet::weight(T::WeightInfo::execute_batch(
		T::MaxActionsPerBatch::get(),
		T::MaxActionsPerBatch::get().saturating_mul(T::MaxMatchesPerAction::get()),
		T::MaxActionsPerBatch::get().saturating_mul(T::MaxMatchesPerAction::get()),
	))]
        pub fn execute_exchange_batch(
            origin: OriginFor<T>,
            actions: BoundedVec<
                SignedExchangeAction<T::Signer, T::Signature, BlockNumberFor<T>>,
                T::MaxActionsPerBatch,
            >,
        ) -> DispatchResultWithPostInfo {
            // The submitter is only a courier; nothing below uses it for authorisation.
            // Unsigned (§12.4): the batch is built by the block proposer, whose consensus key
		// cannot sign an extrinsic. Authorisation comes from each action's own signature, so
		// an open origin grants nothing.
		if let Err(e) = ensure_none(origin) {
			sp_runtime::print(alloc::format!("exchange: batch dispatch: origin rejected: {:?}", e).as_str());
			return Err(e.into())
		}
            ensure!(!actions.is_empty(), Error::<T>::TooManyActions);

            // Deterministic order (§12.3 / S1-3). The key is (class, account, nonce, action
            // hash) — all of it signed or derived from signed bytes — so the proposer that
            // built the batch cannot change the order by rearranging it. `validate_unsigned`
            // refuses a batch that is not already in this order, so the sort here is a
            // formality for anything that reached execution.
            let mut ordered: Vec<(SortKey, usize)> = actions
                .iter()
                .enumerate()
                .map(|(i, a)| (a.sort_key(), i))
                .collect();
            ordered.sort_by_key(|(key, _)| *key);

            let mut total_matches: u32 = 0;
            for (_, i) in ordered {
                let signed = &actions[i];
                let who = <T::Signer as sp_runtime::traits::IdentifyAccount>::into_account(
                    signed.signer.clone(),
                );
                let payload = Self::signed_action_payload(signed);

                // Everything up to and including the nonce is *validation*: it writes nothing
                // but the nonce, and a failure there leaves the account untouched. The
                // business part below is transactional, because `do_place_order` writes as it
                // goes — the maker's account and position first, the taker's and the market's
                // last — and a failure in between used to leave half an order on the book.
                let outcome = (|| -> DispatchResult {
                    // Sub-accounts are keyed into the nonce but not into the trading state
                    // machine, so a non-zero value would authorise one key and apply
                    // another. Refuse it here rather than half-apply it (S0-1).
                    ensure!(
                        signed.subaccount == DEFAULT_SUBACCOUNT,
                        Error::<T>::SubaccountUnsupported
                    );
                    ensure!(
                        sp_runtime::traits::Verify::verify(&signed.signature, &payload[..], &who,),
                        Error::<T>::InvalidActionSignature
                    );
                    // Expiry before the nonce, for the same reason (S1-1).
                    Self::ensure_action_fresh(signed.expires_at)?;
                    let last = ActionNonce::<T>::get(&who, signed.subaccount);
                    ensure!(signed.action_nonce > last, Error::<T>::InvalidActionNonce);
                    // The nonce is consumed whether the business part succeeds or not (S1-2).
                    // A signature that failed must not be replayable — retrying it would
                    // otherwise be indistinguishable from a new one — and the receipt reports
                    // `nonce_consumed` separately from the outcome, so the account can see
                    // which of the two happened.
                    ActionNonce::<T>::insert(&who, signed.subaccount, signed.action_nonce);

                    // The business part, one transaction per action: what it writes is
                    // committed as a whole or not at all, and the next action in the batch
                    // starts from whatever this one committed. The nonce above is outside on
                    // purpose, so a rollback cannot resurrect a consumed signature.
                    let business: Result<u32, DispatchError> =
                        frame_support::storage::with_transaction(|| {
                            let applied: Result<u32, DispatchError> = (|| {
                                match &signed.action {
                                    ExchangeAction::Place(request) => {
                                        let stats = Self::do_place_order(&who, request.clone())?;
                                        Ok(stats.matches)
                                    }
                                    ExchangeAction::Cancel(client_order_id) => {
                                        Self::do_cancel_order(&who, *client_order_id)?;
                                        Ok(0)
                                    }
                                    ExchangeAction::CancelUpTo(client_order_id) => {
                                        let mut account = Self::account_of(&who);
                                        if *client_order_id > account.invalid_before {
                                            account.invalid_before = *client_order_id;
                                            Accounts::<T>::insert(
                                                &who,
                                                signed.subaccount,
                                                account,
                                            );
                                        }
                                        Self::deposit_event(Event::OrdersInvalidatedUpTo {
                                            who: who.clone(),
                                            client_order_id: *client_order_id,
                                        });
                                        Ok(0)
                                    }
                                }
                            })();
                            match applied {
                                Ok(matches) => TransactionOutcome::Commit(Ok(matches)),
                                Err(error) => TransactionOutcome::Rollback(Err(error)),
                            }
                        });

                    if let Ok(matches) = business {
                        total_matches = total_matches.saturating_add(matches);
                    }
                    business.map(|_| ())
                })();

                if let Err(error) = outcome {
                    // Skip, do not abort: a batch holds many users' actions.
                    Self::deposit_event(Event::ExchangeActionFailed {
                        who,
                        subaccount: signed.subaccount,
                        action_nonce: signed.action_nonce,
                        error,
                    });
                }
            }

            let actual = T::WeightInfo::execute_batch(actions.len() as u32, total_matches, 0);
            Self::consume_block_budget(actual)?;
            Ok(PostDispatchInfo {
                actual_weight: Some(actual),
                pays_fee: Pays::Yes,
            })
        }

        /// Lazily removes expired / invalidated / non-reducible orders from the top of the book.
        #[pallet::call_index(6)]
        #[pallet::weight(T::WeightInfo::prune_orders(T::MaxPrunePerCall::get()))]
        pub fn prune_orders(
            origin: OriginFor<T>,
            market_id: MarketId,
            side: Side,
            max_removals: u32,
        ) -> DispatchResultWithPostInfo {
            let _who = ensure_signed(origin)?;
            Self::consume_block_budget(T::WeightInfo::prune_orders(T::MaxPrunePerCall::get()))?;
            let max_removals = max_removals.min(T::MaxPrunePerCall::get());
            let removed = Self::do_prune(market_id, side, max_removals)?;
            let actual = T::WeightInfo::prune_orders(removed);
            Self::consume_block_budget(actual)?;
            Ok(PostDispatchInfo {
                actual_weight: Some(actual),
                pays_fee: Pays::Yes,
            })
        }

        /// Creates a market. Root / governance only.
        #[pallet::call_index(7)]
        #[pallet::weight(T::WeightInfo::create_market())]
        pub fn create_market(origin: OriginFor<T>, config: MarketConfig) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;

            Self::do_create_market(config)
        }

        /// Switches a market between `Active`, `ReduceOnly`, `CancelOnly` and `Paused`.
        #[pallet::call_index(8)]
        #[pallet::weight(T::WeightInfo::set_market_status())]
        pub fn set_market_status(
            origin: OriginFor<T>,
            market_id: MarketId,
            status: MarketStatus,
        ) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;
            Markets::<T>::try_mutate(market_id, |maybe| -> DispatchResult {
                let market = maybe.as_mut().ok_or(Error::<T>::MarketNotFound)?;
                market.status = status;
                Ok(())
            })?;
            Self::deposit_event(Event::MarketStatusChanged { market_id, status });
            Ok(())
        }

        /// Testnet mark price feed. Only updates the price and its timestamp; it never iterates
        /// accounts and never liquidates.
        #[pallet::call_index(9)]
        #[pallet::weight(T::WeightInfo::set_risk_params())]
        pub fn set_risk_params(
            origin: OriginFor<T>,
            market_id: MarketId,
            params: RiskParams,
        ) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;
            ensure!(
                params.initial_margin_ppm > 0 && params.initial_margin_ppm as u128 <= PPM_DENOM,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                params.maintenance_margin_ppm > 0
                    && params.maintenance_margin_ppm <= params.initial_margin_ppm,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                params.maker_fee_ppm as u128 <= PPM_DENOM
                    && params.taker_fee_ppm as u128 <= PPM_DENOM,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                params.max_position_lots > 0,
                Error::<T>::InvalidMarketConfig
            );

            Markets::<T>::try_mutate(market_id, |maybe| -> DispatchResult {
                let market = maybe.as_mut().ok_or(Error::<T>::MarketNotFound)?;
                market.initial_margin_ppm = params.initial_margin_ppm;
                market.maintenance_margin_ppm = params.maintenance_margin_ppm;
                market.maker_fee_ppm = params.maker_fee_ppm;
                market.taker_fee_ppm = params.taker_fee_ppm;
                market.max_position_lots = params.max_position_lots;
                Ok(())
            })?;
            Self::deposit_event(Event::MarketsRiskParamsUpdated { market_id });
            Ok(())
        }

        /// Creates or replaces a fee tier (§9.3).
        #[pallet::call_index(12)]
        #[pallet::weight(T::WeightInfo::set_risk_params())]
        pub fn set_fee_tier(origin: OriginFor<T>, id: FeeTierId, tier: FeeTier) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;
            FeeTiers::<T>::insert(id, tier);
            Self::deposit_event(Event::FeeTierSet { id });
            Ok(())
        }

        /// Assigns a fee tier to a sub-account; `None` reverts to the market's own schedule.
        #[pallet::call_index(13)]
        #[pallet::weight(T::WeightInfo::set_risk_params())]
        pub fn set_account_fee_tier(
            origin: OriginFor<T>,
            who: T::AccountId,
            sub: SubaccountId,
            tier: Option<FeeTierId>,
        ) -> DispatchResult {
            T::ForceOrigin::ensure_origin(origin)?;
            if let Some(id) = tier {
                ensure!(FeeTiers::<T>::contains_key(id), Error::<T>::UnknownFeeTier);
                AccountFeeTier::<T>::insert(&who, sub, id);
            } else {
                AccountFeeTier::<T>::remove(&who, sub);
            }
            Self::deposit_event(Event::AccountFeeTierSet { who, sub, tier });
            Ok(())
        }
    }

    // ------------------------------------------------------------------ internals

    /// Per dispatch matching statistics used to refund weight.
    #[derive(Clone, Copy, Default, Encode, Decode, Eq, PartialEq, TypeInfo, MaxEncodedLen)]
    pub struct MatchStats {
        pub matches: u32,
        pub stale_removed: u32,
        pub level_scans: u32,
    }

    impl<T: Config> Pallet<T> {
        /// Deterministic custody account holding every user's collateral.
        pub fn custody_account() -> T::AccountId {
            T::PalletId::get().into_account_truncating()
        }

        pub(crate) fn now_block() -> BlockNumberFor<T> {
            frame_system::Pallet::<T>::block_number()
        }

        pub(crate) fn account_of(who: &T::AccountId) -> Account {
            Accounts::<T>::get(who, DEFAULT_SUBACCOUNT).unwrap_or_default()
        }

        pub(crate) fn position_of(who: &T::AccountId, market_id: MarketId) -> Position {
            Positions::<T>::get((who, DEFAULT_SUBACCOUNT, market_id)).unwrap_or_default()
        }

        pub(crate) fn params_of(
            market: &Market<BlockNumberFor<T>>,
            mark_price_ticks: PriceTicks,
        ) -> risk::MarketParams {
            risk::MarketParams {
                precision: market.precision(),
                initial_margin_ppm: market.initial_margin_ppm,
                maintenance_margin_ppm: market.maintenance_margin_ppm,
                maker_fee_ppm: market.maker_fee_ppm,
                taker_fee_ppm: market.taker_fee_ppm,
                maker_rebate_ppm: 0,
                mark_price_ticks,
                funding_index: market.funding_index,
            }
        }

        pub(crate) fn math<V>(result: Result<V, MathError>) -> Result<V, DispatchError> {
            result.map_err(|_| Error::<T>::ArithmeticOverflow.into())
        }

        /// Charges one exchange action against the per-block budget (design §11.2).
        ///
        /// `weight` is the **actual** (post-refund) weight for the dispatches that refund, and
        /// the declared weight for the flat ones (where both are identical). The block's own
        /// weight/length limits remain the hard safety boundary; this budget is a *fairness*
        /// control that stops order flow from crowding out staking / governance / EVM traffic.
        /// Measuring it on the declared weight would be so pessimistic (the declared weight
        /// sums worst cases over three independent dimensions) that only a handful of orders
        /// would fit in a block.
        fn consume_block_budget(weight: Weight) -> DispatchResult {
            let now = Self::now_block();
            let mut usage = BlockExchangeUsage::<T>::get().unwrap_or((now, 0u32, Weight::zero()));
            if usage.0 != now {
                usage = (now, 0u32, Weight::zero());
            }
            usage.1 = usage.1.saturating_add(1);
            usage.2 = usage.2.saturating_add(weight);

            let budget = T::ExchangeBlockWeightBudget::get();
            ensure!(
                usage.1 <= T::MaxExchangeActionsPerBlock::get()
                    && usage.2.ref_time() <= budget.ref_time()
                    && usage.2.proof_size() <= budget.proof_size(),
                Error::<T>::ExchangeBlockBudgetExceeded
            );
            BlockExchangeUsage::<T>::put(usage);
            Ok(())
        }

        pub(crate) fn to_balance(amount: QuoteAmount) -> Result<T::Balance, DispatchError> {
            amount
                .try_into()
                .map_err(|_| Error::<T>::ArithmeticOverflow.into())
        }

        #[allow(dead_code)]
        pub(crate) fn to_quote(amount: T::Balance) -> Result<QuoteAmount, DispatchError> {
            amount
                .try_into()
                .map_err(|_| Error::<T>::ArithmeticOverflow.into())
        }

        /// Lazily settles funding and folds realised PnL into `Account::collateral`.
        ///
        /// Must run before any risk decision, withdrawal or fill that touches a position —
        /// otherwise an account could dodge accrued funding or keep withdrawing cash that its
        /// realised losses have already consumed (the H3 hard constraint). Anything that
        /// cannot be collected is booked to `BadDebt`.
        fn settle_party(
            who: &T::AccountId,
            market_id: MarketId,
            account: &mut Account,
            position: &mut Position,
            funding_index: SignedQuote,
        ) -> DispatchResult {
            let unpaid = Self::math(settlement::settle_position(
                account,
                position,
                funding_index,
            ))?;
            if unpaid > 0 {
                // §8.4: the fund is spent first. `cover` is all-or-nothing, so an error means
                // the fund is exhausted. §8.5.1 §2: whatever remains is then recorded
                // explicitly — never absorbed silently, and never implicitly socialised.
                match T::Insurance::cover(unpaid) {
                    Ok(()) => {
                        // The fund paid it, so the outstanding record shrinks with it —
                        // otherwise it stays non-zero forever and keeps licensing ADL.
                        ResidualLossOf::<T>::mutate(who, |loss| {
                            loss.amount = loss.amount.saturating_sub(unpaid)
                        });
                    }
                    Err(_) => {
                        BadDebt::<T>::mutate(|debt| *debt = debt.saturating_add(unpaid));
                        ResidualLossOf::<T>::mutate(who, |loss| {
                            loss.market_id = market_id;
                            // Settlement can run again *after* the position was closed (ADL
                            // folds the realised PnL last), and a flat position has no side.
                            // Keep the last known one rather than overwriting it with `0`,
                            // which is what ADL candidate selection keys off.
                            if position.size_lots != 0 {
                                loss.side_sign = position.size_lots.signum();
                            }
                            loss.amount = loss.amount.saturating_add(unpaid);
                        });
                        Self::deposit_event(Event::ResidualLossRecorded {
                            who: who.clone(),
                            market_id,
                            amount: unpaid,
                        });
                    }
                }
            }
            Ok(())
        }

        /// `Σ |notional| × IMR/MMR` over the sub-account's markets **other than `exclude`** (§9.2).
        ///
        /// The iteration is bounded: it stops as soon as it has seen `MaxClearingMarkets`
        /// positions. `over_limit` reports that the bound was hit, so callers can refuse rather
        /// than silently under-aggregate.
        fn cross_market_requirements(
            who: &T::AccountId,
            sub: SubaccountId,
            exclude: MarketId,
        ) -> Result<(QuoteAmount, QuoteAmount, bool), MathError> {
            let mut initial: QuoteAmount = 0;
            let mut maintenance: QuoteAmount = 0;
            let mut seen = 0u32;
            let bound = T::MaxClearingMarkets::get();

            for (_key, position) in Positions::<T>::iter_prefix((who, sub)) {
                // `iter_prefix` only yields the trailing key, so recover the market id.
                let market_id = _key;
                if market_id == exclude || position.size_lots == 0 {
                    continue;
                }
                let Some(market) = Markets::<T>::get(market_id) else {
                    continue;
                };
                let Ok(mark) = Self::mark_price_of(market_id, &market) else {
                    continue;
                };
                let params = Self::params_of(&market, mark);
                let notional = params
                    .precision
                    .notional(mark, position.size_lots.unsigned_abs())?;
                initial = initial
                    .checked_add(
                        params
                            .precision
                            .apply_ppm_ceil(notional, params.initial_margin_ppm)?,
                    )
                    .ok_or(MathError::Overflow)?;
                maintenance = maintenance
                    .checked_add(
                        params
                            .precision
                            .apply_ppm_ceil(notional, params.maintenance_margin_ppm)?,
                    )
                    .ok_or(MathError::Overflow)?;

                seen = seen.saturating_add(1);
                if seen >= bound {
                    return Ok((initial, maintenance, true));
                }
            }

            Ok((initial, maintenance, false))
        }

        /// `params` with the maker fee schedule replaced by `who`'s fee tier, if it has one
        /// (§9.3). Accounts without a tier keep the market's own schedule.
        fn maker_fee_params_of(
            who: &T::AccountId,
            sub: SubaccountId,
            params: risk::MarketParams,
        ) -> Result<risk::MarketParams, DispatchError> {
            let Some(id) = AccountFeeTier::<T>::get(who, sub) else {
                return Ok(params);
            };
            let tier = FeeTiers::<T>::get(id).ok_or(Error::<T>::UnknownFeeTier)?;
            Ok(risk::MarketParams {
                maker_fee_ppm: tier.maker_fee_ppm,
                taker_fee_ppm: params.taker_fee_ppm,
                maker_rebate_ppm: tier.maker_rebate_ppm,
                ..params
            })
        }

        /// Pays `rebate` out of `FeePool` (§9.3).
        ///
        /// A rebate is a claim on fees the protocol has already collected, so it must never be
        /// funded out of user collateral: an empty pool is an explicit error for governance or
        /// the insurance fund to top up.
        fn charge_maker_rebate(pool: &mut QuoteAmount, rebate: QuoteAmount) -> DispatchResult {
            if rebate == 0 {
                return Ok(());
            }
            *pool = pool
                .checked_sub(rebate)
                .ok_or(Error::<T>::InsufficientFeePool)?;
            Ok(())
        }

        /// Mark price used for every margin / PnL / risk decision.
        ///
        /// The oracle is authoritative (Phase 2 §5.6); there is no fallback.
        ///
        /// Note the asymmetry that matters: when the oracle has **no** fresh price the market
        /// is treated as stale and the caller must refuse to grow risk *and* refuse to
        /// liquidate.
        pub(crate) fn mark_price_of(
            market_id: MarketId,
            market: &Market<BlockNumberFor<T>>,
        ) -> Result<PriceTicks, DispatchError> {
            // The oracle is the only source. When it has no fresh price the market is stale,
            // and the caller must refuse to grow risk *and* refuse to liquidate.
            //
            // This used to fall back to a Root-set cached price. That fallback was dangerous:
            // it returned a genesis seed value whenever the oracle had nothing, so margin and
            // liquidations would silently run on a price that could be arbitrarily wrong.
            T::PriceOracle::mark_price(market_id).ok_or(Error::<T>::MarkPriceStale.into())
        }

        /// Average fill price, in ticks, for a market order of `notional_quote` taken on
        /// `taker_side`.
        ///
        /// Walks price levels from the best opposing price until the notional is covered.
        /// Returns `None` when the book cannot absorb it — dYdX then reports a **zero**
        /// premium rather than falling back to the (unrepresentative) best price, so a thin
        /// book cannot be used to push funding around.
        pub(crate) fn impact_price(
            market: &Market<BlockNumberFor<T>>,
            market_id: MarketId,
            taker_side: Side,
            notional_quote: QuoteAmount,
        ) -> Option<PriceTicks> {
            let book_side = taker_side.opposite();
            let precision = market.precision();
            let max_levels = T::MaxPriceLevelsPerSide::get();

            let mut remaining = notional_quote;
            let mut total_lots: u128 = 0;
            let mut weighted: u128 = 0;
            let mut price = Self::best_price(market, book_side);
            let mut scanned: u32 = 0;

            while let Some(p) = price {
                let level = PriceLevels::<T>::get((market_id, book_side, p))?;
                let per_lot = precision.notional(p, 1).ok()?;
                if per_lot > 0 && level.total_lots > 0 {
                    let needed = (remaining / per_lot) as u64 + 1; // ceil
                    let take = needed.min(level.total_lots);
                    total_lots = total_lots.saturating_add(take as u128);
                    weighted = weighted.saturating_add((take as u128).saturating_mul(p as u128));
                    remaining = remaining.saturating_sub(precision.notional(p, take).ok()?);
                }
                if remaining == 0 {
                    break;
                }
                price = level.next_price;
                scanned = scanned.saturating_add(1);
                if scanned >= max_levels {
                    break;
                }
            }

            if remaining > 0 || total_lots == 0 {
                return None;
            }
            u64::try_from(weighted / total_lots).ok()
        }

        /// Current premium of the perpetual over the index price, in ppm.
        ///
        /// `P = (max(0, ImpactBid − Index) − max(0, Index − ImpactAsk)) / Index`, evaluated
        /// piecewise: only the side the index has crossed is computed (dYdX
        /// `GetPricePremium`). Clamped to `MaxAbsPremiumPpm`.
        pub(crate) fn current_premium_ppm(
            market_id: MarketId,
            market: &Market<BlockNumberFor<T>>,
        ) -> Option<i32> {
            // No fallback: without an index price there is no premium to speak of, and
            // inventing one from a stale cache would put a fabricated number into funding.
            let index = T::PriceOracle::index_price(market_id)?;
            let impact_notional = T::ImpactNotionalQuote::get();

            let premium = if market.best_bid.map_or(false, |b| index < b) {
                // Index below the best bid: the bid side is rich, use the impact bid.
                let impact = Self::impact_price(market, market_id, Side::Sell, impact_notional)?;
                funding::premium_from_impact(index, impact).ok()?
            } else if market.best_ask.map_or(false, |a| index > a) {
                // Index above the best ask: use the impact ask.
                let impact = Self::impact_price(market, market_id, Side::Buy, impact_notional)?;
                funding::premium_from_impact(index, impact).ok()?
            } else {
                // Index inside the spread (or an empty book): no premium.
                0
            };

            let max_abs = T::MaxAbsPremiumPpm::get() as i32;
            Some(premium.max(-max_abs).min(max_abs))
        }

        /// Quote atomic units per price tick, as configured for this market.
        ///
        /// The feed needs this to turn a real price into ticks, and it must be the *chain's*
        /// value: a feed that assumes one tick size while the market uses another publishes
        /// prices that are wrong by orders of magnitude, silently.
        pub fn tick_size_of(market_id: MarketId) -> Option<u128> {
            Markets::<T>::get(market_id).map(|m| m.tick_size_quote)
        }

        /// The premium a proposer would vote for this market right now, in ppm.
        ///
        /// `None` when the book is too thin to say anything — dYdX leaves the premium alone
        /// in that case rather than voting zero, and so do we.
        pub fn premium_vote(market_id: MarketId) -> Option<i32> {
            let market = Markets::<T>::get(market_id)?;
            Self::current_premium_ppm(market_id, &market)
        }

        /// Creates a market. Shared by [`Call::create_market`] and genesis, so the two can
        /// never drift apart in what a "valid market" means.
        fn do_create_market(config: MarketConfig) -> DispatchResult {
            ensure!(config.tick_size_quote > 0, Error::<T>::InvalidMarketConfig);
            ensure!(config.lot_size_base > 0, Error::<T>::InvalidMarketConfig);
            ensure!(config.base_precision > 0, Error::<T>::InvalidMarketConfig);
            let tier = LiquidityTiers::<T>::get(config.liquidity_tier)
                .ok_or(Error::<T>::UnknownLiquidityTier)?;
            ensure!(
                config.max_position_lots > 0,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                tier.initial_margin_ppm > 0 && tier.initial_margin_ppm as u128 <= PPM_DENOM,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                tier.maintenance_margin_ppm > 0
                    && tier.maintenance_margin_ppm <= tier.initial_margin_ppm,
                Error::<T>::InvalidMarketConfig
            );
            ensure!(
                config.maker_fee_ppm as u128 <= PPM_DENOM
                    && config.taker_fee_ppm as u128 <= PPM_DENOM,
                Error::<T>::InvalidMarketConfig
            );

            let market_id = NextMarketId::<T>::get();
            ensure!(
                market_id < T::MaxMarkets::get(),
                Error::<T>::MaxMarketsReached
            );
            ensure!(
                !Markets::<T>::contains_key(market_id),
                Error::<T>::InvariantViolation
            );

            let now = Self::now_block();
            let market = Market {
                base_asset_code: config.base_asset_code,
                quote_asset_code: config.quote_asset_code,
                tick_size_quote: config.tick_size_quote,
                lot_size_base: config.lot_size_base,
                base_precision: config.base_precision,
                initial_margin_ppm: tier.initial_margin_ppm,
                maintenance_margin_ppm: tier.maintenance_margin_ppm,
                liquidity_tier: config.liquidity_tier,
                maker_fee_ppm: config.maker_fee_ppm,
                taker_fee_ppm: config.taker_fee_ppm,
                max_position_lots: config.max_position_lots,
                status: MarketStatus::Active,
                best_bid: None,
                best_ask: None,
                bid_level_count: 0,
                ask_level_count: 0,
                net_oi_lots: 0,
                funding_index: 0,
                last_funding_at: now,
                rate_ppm: 0,
                next_order_id: 1,
            };
            Markets::<T>::insert(market_id, market);
            NextMarketId::<T>::put(
                market_id
                    .checked_add(1)
                    .ok_or(Error::<T>::ArithmeticOverflow)?,
            );
            if PrimaryMarketId::<T>::get().is_none() {
                PrimaryMarketId::<T>::put(market_id);
            }

            Self::deposit_event(Event::MarketCreated { market_id });
            Ok(())
        }

        /// Ends a `funding-sample` epoch: records one premium observation.
        ///
        /// dYdX collects *votes* from proposers every block and takes their median at the
        /// epoch boundary. We have one observation per epoch instead, because votes depend on
        /// proposer injection (P2-D). The vote layer can be added on top without changing the
        /// tick logic; the median of a single value is that value.
        fn process_funding_sample(
            market_id: MarketId,
            market: &Market<BlockNumberFor<T>>,
            _now: BlockNumberFor<T>,
        ) {
            let votes = PremiumVotes::<T>::take(market_id);
            let premium = if votes.is_empty() {
                // No proposer voted this epoch: fall back to one on-chain observation,
                // which is what the median of a single vote would be anyway.
                match Self::current_premium_ppm(market_id, market) {
                    Some(p) => p,
                    None => return,
                }
            } else {
                let mut v = votes.into_inner();
                funding::median(&mut v)
            };
            let max_samples: u32 = <T as Config>::MaxPremiumSamples::get();
            PremiumSamples::<T>::mutate(market_id, |samples| {
                if samples.len() as u32 >= max_samples {
                    samples.remove(0);
                }
                let _ = samples.try_push(premium);
            });
            Self::deposit_event(Event::FundingSampleRecorded {
                market_id,
                premium_ppm: premium,
            });
        }

        /// Ends a `funding-tick` epoch: samples → tail removal → average → clamp → index.
        fn process_funding_tick(
            market_id: MarketId,
            market: &mut Market<BlockNumberFor<T>>,
            now: BlockNumberFor<T>,
        ) {
            let mut samples = PremiumSamples::<T>::get(market_id).into_inner();

            // dYdX pads with zeros when fewer samples than expected were collected (chain
            // halt, longer block times), so a thin history cannot inflate the rate.
            let expected = Self::samples_per_tick() as usize;
            while samples.len() < expected {
                samples.push(0);
            }

            let premium =
                funding::summarize_samples(&mut samples, T::RemovedTailSampleRatioPpm::get());

            let rate = funding::clamp_rate_ppm(
                premium.saturating_add(T::DefaultFundingPpm::get()),
                market.initial_margin_ppm,
                market.maintenance_margin_ppm,
                T::FundingRateClampFactorPpm::get(),
            );
            market.rate_ppm = rate;

            if rate != 0 {
                if let Some(price) = T::PriceOracle::mark_price(market_id) {
                    let per_lot = market.precision().notional(price, 1).unwrap_or(0);
                    let period: u64 = T::FundingRatePeriod::get().saturated_into();
                    let elapsed: u64 = if market.last_funding_at.is_zero() {
                        T::FundingTickPeriod::get().saturated_into()
                    } else {
                        now.saturating_sub(market.last_funding_at).saturated_into()
                    };
                    if let Ok(delta) = funding::funding_index_delta(rate, elapsed, period, per_lot)
                    {
                        market.funding_index = market.funding_index.saturating_add(delta);
                    }
                }
            }

            market.last_funding_at = now;
            PremiumSamples::<T>::remove(market_id);
            Self::deposit_event(Event::FundingSettled {
                market_id,
                rate_ppm: rate,
                funding_index: market.funding_index,
                net_oi_lots: market.net_oi_lots,
            });
        }

        /// Number of sample epochs per tick epoch.
        fn samples_per_tick() -> u64 {
            let tick: u64 = T::FundingTickPeriod::get().saturated_into();
            let sample: u64 = T::FundingSamplePeriod::get().saturated_into();
            if sample == 0 {
                return 0;
            }
            tick.div_ceil(sample)
        }

        /// Lazily moves an account between `Normal` and `Restricted`. Phase 1 never liquidates;
        /// a restricted account may only cancel orders and post reduce-only IOC orders.
        fn refresh_restriction(
            who: &T::AccountId,
            account: &mut Account,
            position: &Position,
            market_id: MarketId,
            params: risk::MarketParams,
        ) -> Result<(), DispatchError> {
            let (oi, om, _) = Self::math(Self::cross_market_requirements(
                who,
                DEFAULT_SUBACCOUNT,
                market_id,
            ))?;
            let view = risk::RiskView::of(account, position, params).with_other_markets(oi, om);
            let equity = Self::math(view.equity())?;
            let maintenance = Self::math(view.maintenance_requirement())?;
            let healthy = equity >= maintenance as SignedQuote;
            match (account.status, healthy) {
                (AccountStatus::Normal, false) => {
                    account.status = AccountStatus::Restricted;
                    Self::deposit_event(Event::AccountRestricted {
                        who: who.clone(),
                        equity,
                        maintenance_margin: maintenance,
                    });
                }
                (AccountStatus::Restricted, true) => {
                    account.status = AccountStatus::Normal;
                    Self::deposit_event(Event::AccountUnrestricted {
                        who: who.clone(),
                        equity,
                    });
                }
                _ => {}
            }
            Ok(())
        }

        fn do_cancel_order(who: &T::AccountId, client_order_id: ClientOrderId) -> DispatchResult {
            let (market_id, order_id) =
                UserOrderByClientId::<T>::get((who, DEFAULT_SUBACCOUNT, client_order_id))
                    .ok_or(Error::<T>::OrderNotFound)?;
            let mut market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
            let order = Orders::<T>::get(market_id, order_id).ok_or(Error::<T>::OrderNotFound)?;
            ensure!(order.owner == *who, Error::<T>::NotOrderOwner);

            let released = Self::remove_order(&mut market, order_id, &order, None)?;
            Markets::<T>::insert(market_id, market);

            Self::deposit_event(Event::OrderCancelled {
                who: who.clone(),
                client_order_id,
                reason: CancelReason::User,
                released_margin: released,
            });
            Ok(())
        }

        fn do_prune(
            market_id: MarketId,
            side: Side,
            max_removals: u32,
        ) -> Result<u32, DispatchError> {
            let mut market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
            let now = Self::now_block();
            let max_levels = T::MaxPriceLevelsPerSide::get();

            let mut removed: u32 = 0;
            let mut levels: u32 = 0;
            let mut price = Self::best_price(&market, side);

            while removed < max_removals && levels < max_levels {
                let Some(p) = price else { break };
                let Some(level) = PriceLevels::<T>::get((market_id, side, p)) else {
                    break;
                };
                price = level.next_price;
                levels = levels.saturating_add(1);

                loop {
                    if removed >= max_removals {
                        break;
                    }
                    let Some(current) = PriceLevels::<T>::get((market_id, side, p)) else {
                        break;
                    };
                    let Some(order_id) = current.head else { break };
                    let order = Orders::<T>::get(market_id, order_id)
                        .ok_or(Error::<T>::InvariantViolation)?;

                    let owner = Self::account_of(&order.owner);
                    let position = Self::position_of(&order.owner, market_id);
                    let expired = order.expires_at <= now;
                    let invalidated = order.client_order_id < owner.invalid_before;
                    let not_reducible =
                        order.reduce_only && position.reduce_capacity(order.side) == 0;

                    if !expired && !invalidated && !not_reducible {
                        break;
                    }
                    let reason = if expired {
                        CancelReason::Expired
                    } else if invalidated {
                        CancelReason::NonceInvalidated
                    } else {
                        CancelReason::Risk
                    };

                    let released = Self::remove_order(&mut market, order_id, &order, None)?;
                    removed = removed.saturating_add(1);
                    Self::deposit_event(Event::OrderCancelled {
                        who: order.owner.clone(),
                        client_order_id: order.client_order_id,
                        reason,
                        released_margin: released,
                    });
                }
            }

            Markets::<T>::insert(market_id, market);
            Ok(removed)
        }

        /// The whole trading hot path for a single order.
        pub(crate) fn do_place_order(
            who: &T::AccountId,
            request: OrderRequest<BlockNumberFor<T>>,
        ) -> Result<MatchStats, DispatchError> {
            let market_id = request.market_id;
            let mut market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;

            // 1. Market status --------------------------------------------------
            match market.status {
                MarketStatus::Active => {}
                MarketStatus::ReduceOnly => {
                    ensure!(request.reduce_only, Error::<T>::MarketReduceOnly)
                }
                MarketStatus::CancelOnly => return Err(Error::<T>::MarketCancelOnly.into()),
                MarketStatus::Paused => return Err(Error::<T>::MarketNotActive.into()),
            }

            // 2. Cheap input validation before any expensive work ---------------
            ensure!(request.price_ticks > 0, Error::<T>::InvalidPrice);
            ensure!(request.quantity_lots > 0, Error::<T>::InvalidQuantity);
            ensure!(
                request.quantity_lots <= market.max_position_lots,
                Error::<T>::InvalidQuantity
            );

            let now = Self::now_block();
            match request.tif {
                TimeInForce::ImmediateOrCancel => {
                    ensure!(request.expires_at == now, Error::<T>::InvalidExpiry)
                }
                TimeInForce::GoodTilCancelled => {
                    ensure!(request.expires_at > now, Error::<T>::InvalidExpiry);
                    let max_expiry = now.saturating_add(T::MaxOrderLifetime::get());
                    ensure!(request.expires_at <= max_expiry, Error::<T>::InvalidExpiry);
                }
            }

            // 3. Account, client order id and flags ------------------------------
            let mut account = Self::account_of(who);
            ensure!(
                request.client_order_id > account.invalid_before,
                Error::<T>::ClientOrderIdInvalidated
            );
            ensure!(
                !UserOrderByClientId::<T>::contains_key((
                    who,
                    DEFAULT_SUBACCOUNT,
                    request.client_order_id
                )),
                Error::<T>::DuplicateClientOrderId
            );
            ensure!(
                !request.post_only || matches!(request.tif, TimeInForce::GoodTilCancelled),
                Error::<T>::PostOnlyRequiresGtc
            );

            let reduce_only_ioc =
                request.reduce_only && matches!(request.tif, TimeInForce::ImmediateOrCancel);
            if matches!(account.status, AccountStatus::Restricted) {
                ensure!(reduce_only_ioc, Error::<T>::AccountRestricted);
            }

            // 4. Mark price must be fresh to grow risk ---------------------------
            let mark = Self::mark_price_of(market_id, &market)?;
            let params = Self::params_of(&market, mark);

            // 5. Position and reduce-only capacity -------------------------------
            let mut position = Self::position_of(who, market_id);
            // Lazy settlement: funding accrued since the last touch plus any realised PnL
            // become cash here, before any risk decision reads them (P2-B §7).
            Self::settle_party(
                who,
                market_id,
                &mut account,
                &mut position,
                market.funding_index,
            )?;
            if request.reduce_only {
                let capacity = position.reduce_capacity(request.side);
                ensure!(capacity > 0, Error::<T>::ReduceOnlyWouldIncrease);
                ensure!(
                    request.quantity_lots <= capacity,
                    Error::<T>::ReduceOnlyWouldIncrease
                );
            }

            // 6. Post-only must not cross ----------------------------------------
            if request.post_only {
                if let Some(best) = Self::opposing_best_price(&market, request.side) {
                    ensure!(
                        !crosses(request.side, request.price_ticks, best),
                        Error::<T>::PostOnlyWouldCross
                    );
                }
            }

            // 7. Reserve the conservative worst case order margin ----------------
            let reserve = Self::math(risk::order_reserve(
                &params,
                request.price_ticks,
                request.quantity_lots,
            ))?;

            let mut taker = PartyState { account, position };

            // 7a. Worst case: the order fills completely at its own limit price.
            // §9.2: refuse rather than under-aggregate when the account spans more markets
            // than the bound allows.
            let (oi, om, over) = Self::math(Self::cross_market_requirements(
                who,
                DEFAULT_SUBACCOUNT,
                market_id,
            ))?;
            ensure!(!over, Error::<T>::TooManyClearingMarkets);
            {
                let mut full = taker.clone();
                Self::math(settlement::apply_position_delta(
                    &mut full.position,
                    signed_lots(request.side, request.quantity_lots),
                    request.price_ticks,
                    &params.precision,
                ))?;
                let fee =
                    Self::math(params.taker_fee_for(request.price_ticks, request.quantity_lots))?;
                full.account.collateral = full
                    .account
                    .collateral
                    .checked_sub(fee)
                    .ok_or(Error::<T>::InsufficientCollateral)?;
                // §9.2: refuse rather than under-aggregate when the account spans more
                // markets than the bound allows.
                let before = risk::RiskView::of(&taker.account, &taker.position, params)
                    .with_other_markets(oi, om);
                let after = risk::RiskView::of(&full.account, &full.position, params)
                    .with_other_markets(oi, om);
                ensure!(
                    Self::math(risk::allows_fill(&before, &after))?,
                    Error::<T>::InsufficientCollateral
                );
            }

            // 7b. A GTC remainder may rest, so the reserved margin must also be covered.
            if matches!(request.tif, TimeInForce::GoodTilCancelled) {
                let mut resting = taker.clone();
                resting.account.reserved_open_order_margin = resting
                    .account
                    .reserved_open_order_margin
                    .checked_add(reserve)
                    .ok_or(Error::<T>::ArithmeticOverflow)?;
                let view = risk::RiskView::of(&resting.account, &resting.position, params)
                    .with_other_markets(oi, om);
                ensure!(
                    Self::math(view.passes_initial_margin())?,
                    Error::<T>::InsufficientCollateral
                );
            }

            taker.account.reserved_open_order_margin = taker
                .account
                .reserved_open_order_margin
                .checked_add(reserve)
                .ok_or(Error::<T>::ArithmeticOverflow)?;

            // 8. Matching loop ----------------------------------------------------
            let mut remaining = request.quantity_lots;
            let mut reserved_remaining = reserve;
            let mut stats = MatchStats::default();
            let mut fee_pool = FeePool::<T>::get();
            let mut net_oi_delta: i64 = 0;
            let max_matches = T::MaxMatchesPerAction::get();
            let maker_side = request.side.opposite();

            while remaining > 0 && stats.matches < max_matches {
                let Some(book_price) = Self::opposing_best_price(&market, request.side) else {
                    break;
                };
                if !crosses(request.side, request.price_ticks, book_price) {
                    break;
                }

                let level = PriceLevels::<T>::get((market_id, maker_side, book_price))
                    .ok_or(Error::<T>::InvariantViolation)?;
                let Some(maker_id) = level.head else {
                    return Err(Error::<T>::InvariantViolation.into());
                };
                let mut maker_order =
                    Orders::<T>::get(market_id, maker_id).ok_or(Error::<T>::InvariantViolation)?;

                let maker_is_taker = maker_order.owner == *who;
                let maker_position = if maker_is_taker {
                    taker.position
                } else {
                    Self::position_of(&maker_order.owner, market_id)
                };
                let maker_invalid_before = if maker_is_taker {
                    taker.account.invalid_before
                } else {
                    Self::account_of(&maker_order.owner).invalid_before
                };

                let expired = maker_order.expires_at <= now;
                let invalidated = maker_order.client_order_id < maker_invalid_before;
                let maker_capacity = if maker_order.reduce_only {
                    maker_position.reduce_capacity(maker_order.side)
                } else {
                    Lots::MAX
                };

                if expired || invalidated || maker_capacity == 0 {
                    let reason = if expired {
                        CancelReason::Expired
                    } else if invalidated {
                        CancelReason::NonceInvalidated
                    } else {
                        CancelReason::Risk
                    };
                    let released = Self::remove_order(
                        &mut market,
                        maker_id,
                        &maker_order,
                        if maker_is_taker {
                            Some((who, &mut taker))
                        } else {
                            None
                        },
                    )?;
                    stats.stale_removed = stats.stale_removed.saturating_add(1);
                    Self::deposit_event(Event::OrderCancelled {
                        who: maker_order.owner.clone(),
                        client_order_id: maker_order.client_order_id,
                        reason,
                        released_margin: released,
                    });
                    if stats.stale_removed >= max_matches {
                        break;
                    }
                    continue;
                }

                let taker_capacity = if request.reduce_only {
                    taker.position.reduce_capacity(request.side)
                } else {
                    Lots::MAX
                };
                if taker_capacity == 0 {
                    break;
                }

                let fill = remaining
                    .min(maker_order.remaining_lots)
                    .min(maker_capacity)
                    .min(taker_capacity);
                if fill == 0 {
                    break;
                }

                let trade_price = book_price;
                let maker_release = if fill == maker_order.remaining_lots {
                    maker_order.reserved_margin
                } else {
                    Self::math(mul_div_ceil_u128(
                        maker_order.reserved_margin as u128,
                        fill as u128,
                        maker_order.remaining_lots as u128,
                    ))?
                };
                let taker_release = if fill == remaining {
                    reserved_remaining
                } else {
                    Self::math(mul_div_ceil_u128(
                        reserved_remaining as u128,
                        fill as u128,
                        remaining as u128,
                    ))?
                };

                let maker_signed = signed_lots(maker_order.side, fill);
                let taker_signed = signed_lots(request.side, fill);
                // A trade is symmetric, so this nets to zero; it is tracked explicitly so the
                // invariant is checked rather than assumed (Phase 2 §6.3).
                net_oi_delta = net_oi_delta
                    .saturating_add(maker_signed)
                    .saturating_add(taker_signed);

                let maker_params =
                    Self::maker_fee_params_of(&maker_order.owner, DEFAULT_SUBACCOUNT, params)?;
                let mut maker_rebate: QuoteAmount = 0;

                if maker_is_taker {
                    // Self trade: both legs settle against the same in-memory state.
                    let mut tmp = taker.clone();
                    let Some(out) = Self::math(risk::simulate_fill(
                        &tmp,
                        maker_signed,
                        trade_price,
                        fill,
                        true,
                        &maker_params,
                        maker_release,
                    ))?
                    else {
                        let released = Self::remove_order(
                            &mut market,
                            maker_id,
                            &maker_order,
                            Some((who, &mut taker)),
                        )?;
                        stats.stale_removed = stats.stale_removed.saturating_add(1);
                        Self::deposit_event(Event::OrderCancelled {
                            who: maker_order.owner.clone(),
                            client_order_id: maker_order.client_order_id,
                            reason: CancelReason::Risk,
                            released_margin: released,
                        });
                        continue;
                    };
                    maker_rebate = maker_rebate.saturating_add(out.rebate);
                    tmp.apply(out);
                    let Some(out2) = Self::math(risk::simulate_fill(
                        &tmp,
                        taker_signed,
                        trade_price,
                        fill,
                        false,
                        &params,
                        taker_release,
                    ))?
                    else {
                        break;
                    };
                    tmp.apply(out2);
                    taker = tmp;
                } else {
                    let mut maker_acct = Self::account_of(&maker_order.owner);
                    let mut maker_pos = maker_position;
                    Self::settle_party(
                        &maker_order.owner,
                        market_id,
                        &mut maker_acct,
                        &mut maker_pos,
                        market.funding_index,
                    )?;
                    let mut maker_state = PartyState {
                        account: maker_acct,
                        position: maker_pos,
                    };
                    let Some(out) = Self::math(risk::simulate_fill(
                        &maker_state,
                        maker_signed,
                        trade_price,
                        fill,
                        true,
                        &maker_params,
                        maker_release,
                    ))?
                    else {
                        let released =
                            Self::remove_order(&mut market, maker_id, &maker_order, None)?;
                        stats.stale_removed = stats.stale_removed.saturating_add(1);
                        Self::deposit_event(Event::OrderCancelled {
                            who: maker_order.owner.clone(),
                            client_order_id: maker_order.client_order_id,
                            reason: CancelReason::Risk,
                            released_margin: released,
                        });
                        continue;
                    };
                    maker_rebate = maker_rebate.saturating_add(out.rebate);
                    maker_state.apply(out);

                    let mut taker_tmp = taker.clone();
                    let Some(out2) = Self::math(risk::simulate_fill(
                        &taker_tmp,
                        taker_signed,
                        trade_price,
                        fill,
                        false,
                        &params,
                        taker_release,
                    ))?
                    else {
                        break;
                    };
                    taker_tmp.apply(out2);

                    let mut maker_account = maker_state.account;
                    Self::refresh_restriction(
                        &maker_order.owner,
                        &mut maker_account,
                        &maker_state.position,
                        market_id,
                        params,
                    )?;
                    // Mark the maker to the current funding index: funding is settled lazily,
                    // so a touched position must never owe the epochs it did not participate in.
                    let mut maker_position = maker_state.position;
                    maker_position.funding_index = market.funding_index;
                    Accounts::<T>::insert(&maker_order.owner, DEFAULT_SUBACCOUNT, maker_account);
                    Positions::<T>::insert(
                        (&maker_order.owner, DEFAULT_SUBACCOUNT, market_id),
                        maker_position,
                    );
                    taker = taker_tmp;
                }

                fee_pool = fee_pool
                    .checked_add(Self::math(maker_params.maker_fee_for(trade_price, fill))?)
                    .and_then(|v| {
                        v.checked_add(Self::math(params.taker_fee_for(trade_price, fill)).ok()?)
                    })
                    .ok_or(Error::<T>::ArithmeticOverflow)?;
                Self::charge_maker_rebate(&mut fee_pool, maker_rebate)?;

                // 8a. Persist the maker order.
                maker_order.remaining_lots = maker_order
                    .remaining_lots
                    .checked_sub(fill)
                    .ok_or(Error::<T>::InvariantViolation)?;
                maker_order.reserved_margin = maker_order
                    .reserved_margin
                    .checked_sub(maker_release)
                    .ok_or(Error::<T>::InvariantViolation)?;
                remaining = remaining
                    .checked_sub(fill)
                    .ok_or(Error::<T>::InvariantViolation)?;
                reserved_remaining = reserved_remaining
                    .checked_sub(taker_release)
                    .ok_or(Error::<T>::InvariantViolation)?;

                if maker_order.remaining_lots == 0 {
                    Self::remove_order(
                        &mut market,
                        maker_id,
                        &maker_order,
                        if maker_is_taker {
                            Some((who, &mut taker))
                        } else {
                            None
                        },
                    )?;
                } else {
                    Orders::<T>::insert(market_id, maker_id, maker_order.clone());
                    PriceLevels::<T>::try_mutate(
                        (market_id, maker_side, book_price),
                        |maybe| -> DispatchResult {
                            let level = maybe.as_mut().ok_or(Error::<T>::InvariantViolation)?;
                            level.total_lots = level
                                .total_lots
                                .checked_sub(fill)
                                .ok_or(Error::<T>::InvariantViolation)?;
                            Ok(())
                        },
                    )?;
                }

                Self::deposit_event(Event::TradeExecuted {
                    market_id,
                    maker: maker_order.owner.clone(),
                    taker: who.clone(),
                    price_ticks: trade_price,
                    lots: fill,
                    maker_order_id: maker_id,
                    taker_client_order_id: request.client_order_id,
                });
                stats.matches = stats.matches.saturating_add(1);
            }

            // 9. Rest, cancel or fill the taker remainder --------------------------
            let still_crossing = match Self::opposing_best_price(&market, request.side) {
                Some(p) => crosses(request.side, request.price_ticks, p),
                None => false,
            };

            if remaining == 0 {
                Self::deposit_event(Event::OrderFilled {
                    who: who.clone(),
                    client_order_id: request.client_order_id,
                });
            } else if matches!(request.tif, TimeInForce::ImmediateOrCancel) {
                // Only IOC cancels its remainder here. A GTC reduce-only remainder is allowed
                // to rest (design §9.4); it loses its fill eligibility later through the maker
                // re-check, which never lets it open a position in the opposite direction.
                let reason = CancelReason::IocRemainder;
                taker.account.reserved_open_order_margin = taker
                    .account
                    .reserved_open_order_margin
                    .checked_sub(reserved_remaining)
                    .ok_or(Error::<T>::InvariantViolation)?;
                Self::deposit_event(Event::OrderCancelled {
                    who: who.clone(),
                    client_order_id: request.client_order_id,
                    reason,
                    released_margin: reserved_remaining,
                });
                reserved_remaining = 0;
            } else if stats.matches >= max_matches && still_crossing {
                taker.account.reserved_open_order_margin = taker
                    .account
                    .reserved_open_order_margin
                    .checked_sub(reserved_remaining)
                    .ok_or(Error::<T>::InvariantViolation)?;
                Self::deposit_event(Event::MatchLimitReached {
                    who: who.clone(),
                    client_order_id: request.client_order_id,
                    remaining_lots: remaining,
                });
                Self::deposit_event(Event::OrderCancelled {
                    who: who.clone(),
                    client_order_id: request.client_order_id,
                    reason: CancelReason::MatchLimitReached,
                    released_margin: reserved_remaining,
                });
                reserved_remaining = 0;
            } else {
                // GTC remainder: risk is re-checked before the order is allowed to rest
                // (design §9.2). Fills already made are kept; when the remainder no longer
                // covers the initial margin requirement it is released and reported as a
                // risk cancellation instead of being queued.
                let view = risk::RiskView::of(&taker.account, &taker.position, params);
                if !Self::math(view.passes_initial_margin())? {
                    taker.account.reserved_open_order_margin = taker
                        .account
                        .reserved_open_order_margin
                        .checked_sub(reserved_remaining)
                        .ok_or(Error::<T>::InvariantViolation)?;
                    Self::deposit_event(Event::OrderCancelled {
                        who: who.clone(),
                        client_order_id: request.client_order_id,
                        reason: CancelReason::Risk,
                        released_margin: reserved_remaining,
                    });
                    reserved_remaining = 0;
                } else {
                    ensure!(
                        taker.account.open_order_count < T::MaxOpenOrdersPerAccount::get(),
                        Error::<T>::TooManyOpenOrders
                    );
                    let order_id = market.next_order_id;
                    market.next_order_id = order_id
                        .checked_add(1)
                        .ok_or(Error::<T>::ArithmeticOverflow)?;
                    taker.account.open_order_count = taker
                        .account
                        .open_order_count
                        .checked_add(1)
                        .ok_or(Error::<T>::ArithmeticOverflow)?;

                    let order = Order {
                        owner: who.clone(),
                        market_id,
                        client_order_id: request.client_order_id,
                        side: request.side,
                        price_ticks: request.price_ticks,
                        original_lots: request.quantity_lots,
                        remaining_lots: remaining,
                        reserved_margin: reserved_remaining,
                        tif: request.tif,
                        post_only: request.post_only,
                        reduce_only: request.reduce_only,
                        placed_at: now,
                        expires_at: request.expires_at,
                        prev_order: None,
                        next_order: None,
                    };
                    let scans = Self::enqueue_order(&mut market, market_id, order_id, order)?;
                    stats.level_scans = stats.level_scans.saturating_add(scans);
                    UserOrderByClientId::<T>::insert(
                        (who, DEFAULT_SUBACCOUNT, request.client_order_id),
                        (market_id, order_id),
                    );
                    Self::deposit_event(Event::OrderAccepted {
                        who: who.clone(),
                        client_order_id: request.client_order_id,
                        market_id,
                        order_id,
                    });
                }
            }

            // 10. Persist the taker and the mutated market ---------------------------
            let mut final_account = taker.account;
            Self::refresh_restriction(who, &mut final_account, &taker.position, market_id, params)?;
            // Same lazy-settlement mark for the taker, and keep net open interest in sync.
            let mut taker_position = taker.position;
            taker_position.funding_index = market.funding_index;
            market.net_oi_lots = funding::apply_net_oi(market.net_oi_lots, net_oi_delta)
                .map_err(|_| Error::<T>::ArithmeticOverflow)?;
            Accounts::<T>::insert(who, DEFAULT_SUBACCOUNT, final_account);
            Positions::<T>::insert((who, DEFAULT_SUBACCOUNT, market_id), taker_position);
            Markets::<T>::insert(market_id, market);
            FeePool::<T>::put(fee_pool);

            Ok(stats)
        }
    }

    // ------------------------------------- controlled entry points for pallet-liquidation

    impl<T: Config> ExchangeEngine<T::AccountId> for Pallet<T> {
        fn liquidate_account(
            keeper: &T::AccountId,
            who: &T::AccountId,
            market_id: MarketId,
            lots: Lots,
        ) -> Result<Lots, DispatchError> {
            Self::do_liquidate_account(who, market_id, lots, keeper)
        }

        fn adl_close(
            who: &T::AccountId,
            market_id: MarketId,
            lots: Lots,
            price_ticks: PriceTicks,
        ) -> Result<(), DispatchError> {
            Self::do_adl_close(who, market_id, lots, price_ticks)
        }

        fn pay_keeper(keeper: &T::AccountId) -> Result<QuoteAmount, DispatchError> {
            let amount = KeeperRebateOf::<T>::take(keeper);
            if amount == 0 {
                return Ok(0);
            }
            let balance = Self::to_balance(amount)?;
            T::Assets::transfer(
                T::CollateralAssetId::get(),
                &Self::custody_account(),
                keeper,
                balance,
                Preservation::Expendable,
            )
            .map_err(|_| Error::<T>::AssetTransferFailed)?;
            Ok(amount)
        }

        fn note_adl_shed(who: &T::AccountId, notional: QuoteAmount) {
            ResidualLossOf::<T>::mutate(who, |loss| {
                loss.shed_notional = loss.shed_notional.saturating_add(notional)
            });
        }

        fn adl_rank(who: &T::AccountId, market_id: MarketId) -> Option<AdlRank> {
            let market = Markets::<T>::get(market_id)?;
            let mark = Self::mark_price_of(market_id, &market).ok()?;
            let params = Self::params_of(&market, mark);
            let account = Self::account_of(who);
            let position = Self::position_of(who, market_id);
            if position.size_lots == 0 {
                return None;
            }

            let view = risk::RiskView::of(&account, &position, params);
            let unrealized = view.unrealized_pnl().ok()?;
            // Return on equity is only meaningful against positive equity; an account with no
            // equity is never a better ADL target than one that is merely less profitable.
            let equity = view.equity().ok()?;
            let roe_ppm = if equity > 0 {
                unrealized.saturating_mul(PPM_DENOM as SignedQuote) / equity
            } else {
                0
            };
            Some(AdlRank {
                size_lots: position.size_lots,
                unrealized_pnl: unrealized,
                roe_ppm: i64::try_from(roe_ppm).unwrap_or(i64::MAX),
                mark_price_ticks: mark,
                notional_per_lot: params.precision.notional(mark, 1).ok()?,
            })
        }

        fn residual_loss_of(who: &T::AccountId) -> ResidualLoss {
            ResidualLossOf::<T>::get(who)
        }
    }

    impl<T: Config> Pallet<T> {
        /// Worst price a liquidation may fill at: the mark moved by `slippage_ppm` against the
        /// closing side. Selling (which closes a long) accepts at most `mark * (1 - s)`.
        fn liquidation_price_bound(
            mark: PriceTicks,
            side: Side,
            slippage_ppm: u32,
        ) -> Result<PriceTicks, MathError> {
            let delta = (mark as u128)
                .checked_mul(slippage_ppm as u128)
                .ok_or(MathError::Overflow)?
                / PPM_DENOM as u128;
            let delta = u64::try_from(delta).map_err(|_| MathError::Overflow)?;
            let bound = match side {
                Side::Sell => mark.saturating_sub(delta),
                Side::Buy => mark.saturating_add(delta),
            };
            Ok(bound.max(1))
        }

        /// Phase 2 §8.3. Closes `lots` — the whole position when `lots == 0` — of an account
        /// that has fallen below its maintenance requirement.
        ///
        /// The close reuses [`Self::do_place_order`] as a reduce-only IOC order bounded by
        /// `MaxLiquidationSlippagePpm`, so a liquidation inherits exactly the same settlement,
        /// margin check and maker re-check as a normal trade instead of growing a second
        /// implementation that could drift away from it.
        fn do_liquidate_account(
            who: &T::AccountId,
            market_id: MarketId,
            requested: Lots,
            keeper: &T::AccountId,
        ) -> Result<Lots, DispatchError> {
            let market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
            // §8.1: never dispose of collateral at a stale or unknown price.
            let mark = Self::mark_price_of(market_id, &market)?;
            let params = Self::params_of(&market, mark);

            let mut account = Self::account_of(who);
            let mut position = Self::position_of(who, market_id);
            // Settle first *and persist it*: this is where an uncollectable funding or PnL
            // shortfall reaches the insurance fund, and the order below must then see the
            // settled state rather than being charged the same funding twice.
            Self::settle_party(
                who,
                market_id,
                &mut account,
                &mut position,
                market.funding_index,
            )?;
            Accounts::<T>::insert(who, DEFAULT_SUBACCOUNT, account.clone());
            Positions::<T>::insert((who, DEFAULT_SUBACCOUNT, market_id), position.clone());

            let size_before = position.size_lots;
            ensure!(size_before != 0, Error::<T>::NotLiquidatable);
            let (oi, om, over) = Self::math(Self::cross_market_requirements(
                who,
                DEFAULT_SUBACCOUNT,
                market_id,
            ))?;
            ensure!(!over, Error::<T>::TooManyClearingMarkets);
            let view = risk::RiskView::of(&account, &position, params).with_other_markets(oi, om);
            ensure!(
                !Self::math(view.passes_maintenance_margin())?,
                Error::<T>::NotLiquidatable
            );

            // §8.5: an account whose equity is already at or below zero cannot pay the fee a
            // book fill requires, so it must never be sent to the order book — it cannot fill
            // and would be left stuck. It is offset at the mark instead, and the insurance fund
            // covers whatever the resulting settlement cannot collect.
            let closed = if Self::math(view.equity())? <= 0 {
                let lots = size_before.unsigned_abs();
                Self::do_adl_close(who, market_id, lots, mark)?;
                lots
            } else {
                let q = if requested == 0 {
                    Self::math(risk::liquidation_lots(
                        &view,
                        T::LiquidationPenaltyPpm::get(),
                        T::MaintenanceBufferPpm::get(),
                        T::MinLiquidationLots::get(),
                    ))?
                } else {
                    requested.min(size_before.unsigned_abs())
                };
                ensure!(q > 0, Error::<T>::NotLiquidatable);

                // A long is closed by selling, a short by buying.
                let side = if size_before > 0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let bound = Self::math(Self::liquidation_price_bound(
                    mark,
                    side,
                    T::MaxLiquidationSlippagePpm::get(),
                ))?;

                let request = OrderRequest {
                    market_id,
                    // A liquidation never rests, so this id is never recorded against the
                    // account.
                    client_order_id: ClientOrderId::MAX,
                    side,
                    price_ticks: bound,
                    quantity_lots: q,
                    tif: TimeInForce::ImmediateOrCancel,
                    post_only: false,
                    reduce_only: true,
                    expires_at: Self::now_block(),
                };

                // Charge what the fill actually cost, not the configured worst case.
                // Liquidations are permissionless and tend to arrive in bursts, so paying the
                // maximum every time would exhaust the block budget and lock *further*
                // liquidations out — exactly the liveness failure the budget exists to prevent.
                let stats = Self::do_place_order(who, request)?;
                Self::consume_block_budget(T::WeightInfo::liquidate_account(stats.matches))?;

                let after = Self::position_of(who, market_id);
                let closed = size_before
                    .unsigned_abs()
                    .saturating_sub(after.size_lots.unsigned_abs());
                ensure!(closed > 0, Error::<T>::LiquidationSlippageExceeded);
                closed
            };

            // §8.3 step 6: the penalty is charged on the closed notional. §8.4: the fund is
            // credited before anything is socialised to profitable traders.
            let notional = Self::math(params.precision.notional(mark, closed))?;
            let penalty = Self::math(
                params
                    .precision
                    .apply_ppm_ceil(notional, T::LiquidationPenaltyPpm::get()),
            )?;
            let rebate = Self::math(
                params
                    .precision
                    .apply_ppm_ceil(notional, T::LiquidationKeeperRebatePpm::get()),
            )?;
            // Only what the account can actually pay may be handed out. Distributing the
            // configured penalty in full would move real tokens out of custody that nobody
            // ever paid, and the difference would come out of *other* users' collateral —
            // the implicit socialisation §8.5 §3 forbids.
            let mut account_after = Self::account_of(who);
            let charged = penalty.min(account_after.collateral);
            let rebate = rebate.min(charged);
            let to_insurance = charged.saturating_sub(rebate);
            account_after.collateral = account_after.collateral.saturating_sub(charged);
            Accounts::<T>::insert(who, DEFAULT_SUBACCOUNT, account_after);
            KeeperRebateOf::<T>::mutate(keeper, |pool| *pool = pool.saturating_add(rebate));

            // Failure to move the money must not revert the liquidation itself.
            let credited = T::Insurance::credit(to_insurance).is_ok();

            Self::deposit_event(Event::PositionLiquidated {
                who: who.clone(),
                market_id,
                lots: closed,
                penalty: charged,
                insurance_delta: if credited { to_insurance } else { 0 },
            });
            Ok(closed)
        }

        /// §8.5: offsets `lots` of a bankrupt position at the ADL `price`, settling before and
        /// after so the result is left in cash terms.
        fn do_adl_close(
            who: &T::AccountId,
            market_id: MarketId,
            lots: Lots,
            price_ticks: PriceTicks,
        ) -> DispatchResult {
            let market = Markets::<T>::get(market_id).ok_or(Error::<T>::MarketNotFound)?;
            let mut account = Self::account_of(who);
            let mut position = Self::position_of(who, market_id);
            Self::settle_party(
                who,
                market_id,
                &mut account,
                &mut position,
                market.funding_index,
            )?;
            ensure!(position.size_lots != 0, Error::<T>::NotLiquidatable);

            let closed = lots.min(position.size_lots.unsigned_abs());
            let signed = if position.size_lots > 0 {
                -(closed as i64)
            } else {
                closed as i64
            };
            Self::math(settlement::apply_position_delta(
                &mut position,
                signed,
                price_ticks,
                &market.precision(),
            ))?;
            // Fold what the offset just realised, leaving the account in cash terms.
            Self::settle_party(
                who,
                market_id,
                &mut account,
                &mut position,
                market.funding_index,
            )?;
            // That settle runs on a now-flat position, so it cannot tell which side the loss
            // came from. Record it here while it is still known: ADL candidate selection keys
            // off `side_sign`, and a `0` there silently disables the direction filter (§8.5).
            let side = if position.size_lots > 0 {
                -1
            } else if position.size_lots < 0 {
                1
            } else {
                0
            };
            if side != 0 || true {
                let known_side = if signed < 0 {
                    1
                } else if signed > 0 {
                    -1
                } else {
                    0
                };
                if known_side != 0 {
                    ResidualLossOf::<T>::mutate(who, |loss| {
                        if loss.amount > 0 && loss.side_sign == 0 {
                            loss.side_sign = known_side
                        }
                    });
                }
            }

            Accounts::<T>::insert(who, DEFAULT_SUBACCOUNT, account);
            Positions::<T>::insert((who, DEFAULT_SUBACCOUNT, market_id), position);
            Self::deposit_event(Event::AdlClosed {
                who: who.clone(),
                market_id,
                lots: closed,
                price_ticks,
            });
            // Charged here rather than at the call sites so that *every* path pays for it.
            // `Liquidation::adl` used to skip the budget entirely, letting one block carry an
            // unlimited number of forced reductions.
            Self::consume_block_budget(T::WeightInfo::adl_close())?;
            Ok(())
        }
    }

    // ------------------------------------------- node-side pool support (P2-D §12.2)

    impl<T: Config> Pallet<T> {
        /// Pre-validation for the exchange transaction pool.
        ///
        /// Purely advisory — it exists to give users fast feedback and to keep junk out of
        /// the pool. The runtime replay inside `execute_exchange_batch` is the only
        /// authority: a bug in the node must never be able to smuggle an action through.
        /// Pre-validation for premium votes. Advisory, like every other pre-check here.
    fn validate_premium_votes(call: &Call<T>) -> TransactionValidity {
        let Call::submit_premium_votes { market_id, votes, nonce } = call else {
            return InvalidTransaction::Call.into()
        };
        if votes.is_empty() || votes.len() as u32 > T::MaxPremiumVotes::get() {
            return InvalidTransaction::Call.into()
        }
        // A nonce that is not ahead of the last accepted one is a replay. Letting it
        // through is what filled the pool with byte-identical votes, each of which the
        // pool then banned after the block that contained it was imported.
        if *nonce <= PremiumVoteNonce::<T>::get(market_id) {
            return InvalidTransaction::Stale.into()
        }
        ValidTransaction::with_tag_prefix("ExchangePremium")
            .priority(1)
            .longevity(1)
            .propagate(false)
            .and_provides((b"premium-votes", *market_id))
            .build()
    }

    /// The bytes an action's signature covers (S1-1).
    ///
    /// One function, called by the dispatchable, by `validate_signed_action` and by every
    /// off-chain caller: a payload assembled in two places drifts the moment one side
    /// changes, and the failure is a signature that verifies nowhere. The oracle's
    /// `vote_payload` is the same pattern.
    ///
    /// It binds, in order: the domain and version (so the format itself is part of the
    /// bytes), the chain (the genesis hash, so the signature is good here and nowhere
    /// else), the sub-account, the nonce, the expiry, and the action.
    pub fn signed_action_payload(
        action: &SignedExchangeAction<T::Signer, T::Signature, BlockNumberFor<T>>,
    ) -> Vec<u8> {
        let genesis = frame_system::Pallet::<T>::block_hash(BlockNumberFor::<T>::zero());
        (
            SIGNED_ACTION_DOMAIN,
            SIGNED_ACTION_VERSION,
            genesis,
            action.subaccount,
            action.action_nonce,
            action.expires_at,
            &action.action,
        )
            .encode()
    }

    /// A signature is valid only inside its own window (S1-1).
    ///
    /// Bounded above by `MaxOrderLifetime` for the same reason a GTC order is: an expiry
    /// arbitrarily far ahead is a signature with no expiry at all, and the nonce is no
    /// substitute — it prevents *reuse*, not *late use*.
    pub(crate) fn ensure_action_fresh(expires_at: BlockNumberFor<T>) -> DispatchResult {
        let now = frame_system::Pallet::<T>::block_number();
        ensure!(expires_at > now, Error::<T>::ActionExpired);
        ensure!(
            expires_at <= now.saturating_add(T::MaxOrderLifetime::get()),
            Error::<T>::InvalidActionExpiry
        );
        Ok(())
    }

    pub fn validate_signed_action(
            action: &SignedExchangeAction<T::Signer, T::Signature, BlockNumberFor<T>>,
        ) -> DispatchResult {
            let who = <T::Signer as sp_runtime::traits::IdentifyAccount>::into_account(
                action.signer.clone(),
            );
            // The pool must not accept what the dispatchable will refuse (S0-1).
            ensure!(
                action.subaccount == DEFAULT_SUBACCOUNT,
                Error::<T>::SubaccountUnsupported
            );
            let payload = Self::signed_action_payload(action);
            ensure!(
                sp_runtime::traits::Verify::verify(&action.signature, &payload[..], &who),
                Error::<T>::InvalidActionSignature
            );
            // Expiry before the nonce: an expired signature is a leaked signature, and
            // refusing it must not cost the account anything.
            Self::ensure_action_fresh(action.expires_at)?;
            let last = ActionNonce::<T>::get(&who, action.subaccount);
            ensure!(action.action_nonce > last, Error::<T>::InvalidActionNonce);
            Ok(())
        }

        /// Worst-case weight of one action, so the pool can price it before inclusion.
        pub fn action_weight_of(
            action: &ExchangeAction<BlockNumberFor<T>>,
        ) -> frame_support::weights::Weight {
            match action {
                ExchangeAction::Place(_) => T::WeightInfo::place_order(
                    T::MaxMatchesPerAction::get(),
                    T::MaxMatchesPerAction::get(),
                    T::MaxPriceLevelsPerSide::get(),
                ),
                ExchangeAction::Cancel(_) => T::WeightInfo::cancel_order(),
                ExchangeAction::CancelUpTo(_) => T::WeightInfo::cancel_up_to(),
            }
        }
    }

    // ------------------------------------------- settlement-aware conservation

    impl<T: Config> Pallet<T> {
        /// PnL that has been *earned* but not yet folded into collateral.
        ///
        /// Settlement is lazy by design: realised PnL sits on the position until the next
        /// touch, and unrealised PnL is never folded at all. Both are still claims on
        /// custody, so a conservation check that only sums `collateral` is wrong for as
        /// long as any position is open — it makes a realised loss look like money that
        /// vanished, which is exactly what it reported during the §18.6 fault injection.
        #[allow(dead_code)]
        pub fn unsettled_pnl() -> SignedQuote {
            let mut total: SignedQuote = 0;
            for ((who, _subaccount, market_id), position) in Positions::<T>::iter() {
                total = total.saturating_add(position.realized_pnl);
                let Some(market) = Markets::<T>::get(market_id) else {
                    continue;
                };
                let Ok(mark) = Self::mark_price_of(market_id, &market) else {
                    continue;
                };
                let account = Self::account_of(&who);
                let view = risk::RiskView::of(&account, &position, Self::params_of(&market, mark));
                if let Ok(pnl) = view.unrealized_pnl() {
                    total = total.saturating_add(pnl);
                }
            }
            total
        }
    }

    // ------------------------------------------------------------------ test helpers

    impl<T: Config> Pallet<T> {
        /// `Assets::balance(collateral, custody) == Σ collateral + FeePool`.
        ///
        /// The pallet has no way to iterate accounts, so this is checked directly in tests.
        #[allow(dead_code)]
        pub fn custody_balance() -> QuoteAmount {
            Self::to_quote(T::Assets::balance(
                T::CollateralAssetId::get(),
                &Self::custody_account(),
            ))
            .unwrap_or_default()
        }

        /// Sum of every recorded internal collateral balance plus the fee pool.
        ///
        /// P2-B: when `BadDebt > 0` custody has paid out funding it could not collect, so the
        /// identity becomes `custody + BadDebt == Σ collateral + FeePool`. `pallet-insurance`
        /// (step 2) must clear `BadDebt` and restore the plain form.
        #[allow(dead_code)]
        pub fn internal_liabilities() -> QuoteAmount {
            let mut total = FeePool::<T>::get();
            for (_, rebate) in KeeperRebateOf::<T>::iter() {
                total = total.saturating_add(rebate);
            }
            for (_, _, account) in Accounts::<T>::iter() {
                total = total.saturating_add(account.collateral);
            }
            let settled = total.saturating_sub(BadDebt::<T>::get());
            let pnl = Self::unsettled_pnl();
            if pnl.is_negative() {
                settled.saturating_sub(pnl.unsigned_abs())
            } else {
                settled.saturating_add(pnl as QuoteAmount)
            }
        }

        /// Uncollected losses; must be covered by the insurance fund before it can be non-zero in
        /// production.
        #[allow(dead_code)]
        pub fn bad_debt() -> QuoteAmount {
            BadDebt::<T>::get()
        }

        /// Walks a price level FIFO exactly once and returns the order ids in queue order.
        #[allow(dead_code)]
        pub fn level_queue(market_id: MarketId, side: Side, price: PriceTicks) -> Vec<OrderId> {
            let mut out = Vec::new();
            let Some(level) = PriceLevels::<T>::get((market_id, side, price)) else {
                return out;
            };
            let mut cursor = level.head;
            while let Some(id) = cursor {
                out.push(id);
                match Orders::<T>::get(market_id, id) {
                    Some(order) => cursor = order.next_order,
                    None => break,
                }
            }
            out
        }

        /// Walks the price chain from the best price and returns the prices in order.
        #[allow(dead_code)]
        pub fn price_chain(market_id: MarketId, side: Side) -> Vec<PriceTicks> {
            let mut out = Vec::new();
            let Some(market) = Markets::<T>::get(market_id) else {
                return out;
            };
            let mut cursor = Self::best_price(&market, side);
            let mut guard = 0u32;
            while let Some(p) = cursor {
                out.push(p);
                match PriceLevels::<T>::get((market_id, side, p)) {
                    Some(level) => cursor = level.next_price,
                    None => break,
                }
                guard = guard.saturating_add(1);
                if guard > T::MaxPriceLevelsPerSide::get() {
                    break;
                }
            }
            out
        }
    }
}
