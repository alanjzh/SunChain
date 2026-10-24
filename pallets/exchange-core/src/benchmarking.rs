//! Benchmarks for `pallet-exchange-core`.
//!
//! The hot path benchmarks (`place_order*`, `cancel_order`, `execute_batch`, `prune_orders`)
//! are the ones that matter for the Phase 1 block budget. Governance calls keep their
//! hand written weights until step 8 of the implementation plan.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use crate::{
    types::{Market, MarketId, MarketStatus, Side, TimeInForce},
    Config, Markets, NextMarketId, Pallet, PrimaryMarketId,
};
use codec::Encode;
use exchange_primitives::{ExchangeEngine, PriceOracle, DEFAULT_SUBACCOUNT};
use frame_benchmarking::v2::*;
use frame_support::{
    traits::{
        tokens::{fungibles::Mutate, Preservation},
        EnsureOrigin, Get, Hooks,
    },
    BoundedVec,
};
use frame_system::RawOrigin;
use sp_std::vec::Vec;
use sp_runtime::{
    traits::{Bounded, One},
    SaturatedConversion,
};

const BENCH_MARKET: MarketId = 0;

/// A price source used **only** while benchmarking.
///
/// The real oracle's trait is read-only, so a generic benchmark cannot seed it — and the
/// Root-set cache that used to serve this purpose was removed because it let margin and
/// liquidation run on a stale, arbitrary price. The runtime swaps this in under
/// `runtime-benchmarks` so the core still has a price to work with.
pub struct BenchmarkOracle;

const BENCH_ORACLE_KEY: &[u8] = b":sun:bench-oracle:";

impl BenchmarkOracle {
	fn read() -> Vec<(MarketId, PriceTicks)> {
		sp_io::storage::get(BENCH_ORACLE_KEY)
			.and_then(|raw| codec::Decode::decode(&mut &raw[..]).ok())
			.unwrap_or_default()
	}

	pub fn set_price(market: MarketId, price: PriceTicks) {
		let mut prices = Self::read();
		prices.retain(|(m, _)| *m != market);
		prices.push((market, price));
		sp_io::storage::set(BENCH_ORACLE_KEY, &prices.encode());
	}

	fn get(market: MarketId) -> Option<PriceTicks> {
		Self::read().iter().find(|(m, _)| *m == market).map(|(_, p)| *p)
	}
}

impl PriceOracle<MarketId> for BenchmarkOracle {
	fn index_price(market: MarketId) -> Option<PriceTicks> {
		Self::get(market)
	}
	fn mark_price(market: MarketId) -> Option<PriceTicks> {
		Self::get(market)
	}
	fn is_fresh(market: MarketId) -> bool {
		Self::get(market).is_some()
	}
}

const MARK: PriceTicks = 60_000;
const TICK: QuoteAmount = 1_000_000;
const LOT: u64 = 1_000;
const PRECISION: u64 = 1_000_000_000;
const DEPOSIT: QuoteAmount = 1_000_000_000_000u128;

fn bench_market<T: Config>() -> Market<frame_system::pallet_prelude::BlockNumberFor<T>> {
    Market {
        // Margins normally come from the tier; the benchmark builds the struct directly.
        initial_margin_ppm: 50_000,
        maintenance_margin_ppm: 25_000,
        base_asset_code: *b"BTC     ",
        quote_asset_code: *b"USD     ",
        tick_size_quote: TICK,
        lot_size_base: LOT,
        base_precision: PRECISION,
        liquidity_tier: 0,
        maker_fee_ppm: 100,
        taker_fee_ppm: 500,
        max_position_lots: 10_000_000,
        status: MarketStatus::Active,
        best_bid: None,
        best_ask: None,
        bid_level_count: 0,
        ask_level_count: 0,
        net_oi_lots: 0,
        funding_index: 0,
        last_funding_at: frame_system::Pallet::<T>::block_number(),
        rate_ppm: 0,
        next_order_id: 1,
    }
}

/// Installs a fresh market directly in storage so that benchmarks never depend on the
/// governance origin.
fn install_market<T: Config>() {
    Markets::<T>::insert(BENCH_MARKET, bench_market::<T>());
    NextMarketId::<T>::put(BENCH_MARKET + 1);
    PrimaryMarketId::<T>::put(BENCH_MARKET);
}

/// The same market, on a given tier, so a tier refresh has something to refresh (S1-4).
fn bench_market_with_tier<T: Config>(
    market_id: MarketId,
    tier_id: u32,
) -> Market<frame_system::pallet_prelude::BlockNumberFor<T>> {
    let mut market = bench_market::<T>();
    market.liquidity_tier = tier_id;
    let _ = market_id;
    market
}

fn to_balance<T: Config>(amount: QuoteAmount) -> Result<T::Balance, BenchmarkError> {
    amount
        .try_into()
        .map_err(|_| BenchmarkError::Stop("balance conversion"))
}

fn fund_with<T: Config>(who: &T::AccountId, amount: QuoteAmount) -> Result<(), BenchmarkError> {
    let balance = to_balance::<T>(amount)?;
    let _ = T::Assets::mint_into(T::CollateralAssetId::get(), who, balance)
        .map_err(|_| BenchmarkError::Stop("mint"))?;
    Pallet::<T>::deposit_collateral(RawOrigin::Signed(who.clone()).into(), 0, amount)
        .map_err(|_| BenchmarkError::Stop("deposit"))?;
    Ok(())
}

fn fund<T: Config>(who: &T::AccountId) -> Result<(), BenchmarkError> {
    fund_with::<T>(who, DEPOSIT)
}

/// Seeds an account's *internal* collateral directly, bypassing the asset pallet.
///
/// `fund()` goes through `deposit_collateral`, which mints the collateral asset and moves real
/// tokens into custody. That needs the asset to exist, and several benchmark entry points turn
/// out not to have it — they fail with `UnknownAsset` (see the work log). The risk engine only
/// ever reads `Accounts`, so writing it directly is enough for any benchmark that does not
/// itself move collateral in or out of custody.
fn setup_account<T: Config>(who: &T::AccountId, collateral: QuoteAmount) {
    crate::Accounts::<T>::insert(
        who,
        DEFAULT_SUBACCOUNT,
        Account {
            collateral,
            ..Default::default()
        },
    );
}

fn resting_request<T: Config>(
    client: ClientOrderId,
    side: Side,
    price: PriceTicks,
    qty: Lots,
) -> OrderRequest<frame_system::pallet_prelude::BlockNumberFor<T>> {
    OrderRequest {
        market_id: BENCH_MARKET,
        client_order_id: client,
        side,
        price_ticks: price,
        quantity_lots: qty,
        tif: TimeInForce::GoodTilCancelled,
        post_only: false,
        reduce_only: false,
        expires_at: frame_system::Pallet::<T>::block_number() + 10_000u32.into(),
    }
}

#[benchmarks]
mod benchmarks {
    use super::*;

    #[benchmark]
    fn deposit_collateral() {
        let caller: T::AccountId = whitelisted_caller();
        let amount = to_balance::<T>(DEPOSIT).unwrap();
        let _ = T::Assets::mint_into(T::CollateralAssetId::get(), &caller, amount).unwrap();

        #[extrinsic_call]
        deposit_collateral(RawOrigin::Signed(caller), 0, DEPOSIT);
    }

    #[benchmark]
    fn withdraw_collateral() {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        fund::<T>(&caller).unwrap();

        #[extrinsic_call]
        withdraw_collateral(RawOrigin::Signed(caller), 0, 1u128);
    }

    /// Taker rests at a brand new price level after walking `l` existing levels of the chain.
    #[benchmark]
    fn place_order_rest_new_level(l: Linear<0, 32>) {
        let maker: T::AccountId = account("maker", 0, 0);
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&maker, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        let l = l
            .min(T::MaxPriceLevelsPerSide::get())
            .min(T::MaxOpenOrdersPerAccount::get());
        // `l` resting bids *above* the insert price, so `ensure_level` has to walk the whole
        // price chain before it can link the new level in.
        for i in 0..l {
            Pallet::<T>::place_order(
                RawOrigin::Signed(maker.clone()).into(),
                resting_request::<T>(i as ClientOrderId + 1, Side::Buy, 60_000 - i as u64, 10),
            )
            .unwrap();
        }
        let request = resting_request::<T>(1, Side::Buy, 50_000, 10);

        #[extrinsic_call]
        place_order(RawOrigin::Signed(caller), 0, request);
    }

    /// Taker sweeps `s` stale (invalidated) resting maker orders out of the book.
    #[benchmark]
    fn place_order_stale_removal(s: Linear<1, 32>) {
        let maker: T::AccountId = account("maker", 0, 0);
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&maker, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        let s = s
            .min(T::MaxMatchesPerAction::get())
            .min(T::MaxOrdersPerLevel::get());
        for i in 0..s {
            Pallet::<T>::place_order(
                RawOrigin::Signed(maker.clone()).into(),
                resting_request::<T>(i as ClientOrderId + 1, Side::Sell, 60_000, 10),
            )
            .unwrap();
        }
        // Logically invalidate every maker order: they stay in the book until a taker sweeps
        // them, which is exactly the stale-removal path of the matching loop.
        Pallet::<T>::cancel_up_to(RawOrigin::Signed(maker).into(), 0, s as ClientOrderId + 1).unwrap();

        let request = resting_request::<T>(1, Side::Buy, 60_000, 10u64 * s as u64 + 10);

        #[extrinsic_call]
        place_order(RawOrigin::Signed(caller), 0, request);
    }

    #[benchmark]
    fn place_order_existing_level() {
        let maker: T::AccountId = account("maker", 0, 0);
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&maker, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        Pallet::<T>::place_order(
            RawOrigin::Signed(maker).into(), 0,
            resting_request::<T>(1, Side::Sell, 61_000, 100),
        )
        .unwrap();
        let request = resting_request::<T>(1, Side::Sell, 61_000, 100);

        #[extrinsic_call]
        place_order(RawOrigin::Signed(caller), 0, request);
    }

    /// Taker sweeping `m` resting maker orders.
    #[benchmark]
    fn place_order_matches(m: Linear<1, 32>) {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&caller, DEPOSIT);
        let m = m.min(T::MaxMatchesPerAction::get());
        for i in 0..m {
            let maker: T::AccountId = account("maker", i, 0);
            setup_account::<T>(&maker, DEPOSIT);
            Pallet::<T>::place_order(
                RawOrigin::Signed(maker).into(), 0,
                resting_request::<T>(1, Side::Sell, 60_000, 10),
            )
            .unwrap();
        }
        let request = resting_request::<T>(1, Side::Buy, 60_000, 10u64 * m as u64);

        #[extrinsic_call]
        place_order(RawOrigin::Signed(caller), 0, request);
    }

    /// Liquidating an underwater account into `m` resting maker orders (§8.3, book path).
    ///
    /// The victim is deliberately *just* above its initial margin, so marking the price from
    /// 60_000 down to 57_000 drops it below maintenance while leaving equity positive — which
    /// is what makes the close go through `do_place_order` rather than the bankruptcy path.
    #[benchmark]
    fn liquidate_account(m: Linear<1, 32>) -> Result<(), BenchmarkError> {
        let m = m.min(T::MaxMatchesPerAction::get());
        let qty: Lots = 10 * m as u64;
        install_market::<T>();

        // Liquidity for the liquidation to sell into.
        for i in 0..m {
            let maker: T::AccountId = account("maker", i, 0);
            setup_account::<T>(&maker, DEPOSIT);
            Pallet::<T>::place_order(
                RawOrigin::Signed(maker).into(), 0,
                resting_request::<T>(1, Side::Buy, 57_000, 10),
            )
            .unwrap();
        }

        // Someone to sell to the victim so it can open a long at 60_000.
        let seller: T::AccountId = account("seller", 0, 0);
        setup_account::<T>(&seller, DEPOSIT);
        Pallet::<T>::place_order(
            RawOrigin::Signed(seller).into(), 0,
            resting_request::<T>(1, Side::Sell, 60_000, qty),
        )
        .unwrap();

        // The victim: 31_000 per 10 lots covers the 30_000 initial margin (5% of a
        // 600_000 notional) and nothing more.
        let victim: T::AccountId = account("victim", 0, 0);
        setup_account::<T>(&victim, 31_000u128 * m as u128);
        Pallet::<T>::place_order(
            RawOrigin::Signed(victim.clone()).into(),
            resting_request::<T>(1, Side::Buy, 60_000, qty),
        )
        .unwrap();

        // Mark down — this is what makes the victim liquidatable.
        //
        // Mutate the price fields only: replacing the whole `Market` would also reset
        // `best_bid` / `best_ask` / the level counts and throw away the liquidity we just
        // placed, which shows up as `LiquidationSlippageExceeded`.
        BenchmarkOracle::set_price(BENCH_MARKET, 57_000);

        #[block]
        {
            Pallet::<T>::liquidate_account(&victim, &victim, BENCH_MARKET, 0)
                .map_err(|_| BenchmarkError::Stop("liquidate_account"))?;
        }

        Ok(())
    }

    /// Force-offsetting a bankrupt position at the mark, without the order book (§8.5).
    #[benchmark]
    fn adl_close() -> Result<(), BenchmarkError> {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        let seller: T::AccountId = account("seller", 0, 0);
        setup_account::<T>(&seller, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        Pallet::<T>::place_order(
            RawOrigin::Signed(seller).into(), 0,
            resting_request::<T>(1, Side::Sell, 60_000, 1_000),
        )
        .unwrap();
        Pallet::<T>::place_order(
            RawOrigin::Signed(caller.clone()).into(),
            resting_request::<T>(1, Side::Buy, 60_000, 1_000),
        )
        .unwrap();

        // caller is long 1_000 @ 60_000; marking down to 57_000 leaves it deeply underwater.
        let mut market = bench_market::<T>();
        Markets::<T>::insert(BENCH_MARKET, market);

        #[block]
        {
            Pallet::<T>::adl_close(&caller, BENCH_MARKET, 1_000, 57_000)
                .map_err(|_| BenchmarkError::Stop("adl_close"))?;
        }

        Ok(())
    }

    #[benchmark]
    fn cancel_order() {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&caller, DEPOSIT);
        Pallet::<T>::place_order(
            RawOrigin::Signed(caller.clone()).into(),
            resting_request::<T>(1, Side::Buy, 59_000, 100),
        )
        .unwrap();

        #[extrinsic_call]
        cancel_order(RawOrigin::Signed(caller), 0, 1);
    }

    #[benchmark]
    fn cancel_up_to() {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&caller, DEPOSIT);

        #[extrinsic_call]
        cancel_up_to(RawOrigin::Signed(caller), 0, 100);
    }

    #[benchmark]
    fn execute_batch(a: Linear<1, 8>) {
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&caller, DEPOSIT);
        let a = a.min(T::MaxActionsPerBatch::get());
        let mut actions = alloc::vec::Vec::new();
        for i in 0..a {
            actions.push(ExchangeAction::Place(resting_request::<T>(
                i as ClientOrderId + 1,
                Side::Buy,
                50_000 + i as u64,
                10,
            )));
        }
        let actions: BoundedVec<_, T::MaxActionsPerBatch> = BoundedVec::truncate_from(actions);

        #[extrinsic_call]
        execute_batch(RawOrigin::Signed(caller), 0, actions);
    }

    /// The same batch, but every action *matches*: a resting sell is on the book first, so the
    /// matches / stale / level-scan dimensions of the weight are exercised rather than only
    /// the resting path (S1-4). Without it the synthesised weight covered only a batch of
    /// orders that rest.
    #[benchmark]
    fn execute_batch_matching(a: Linear<1, 8>) {
        let maker: T::AccountId = account("maker", 0, 0);
        let caller: T::AccountId = whitelisted_caller();
        install_market::<T>();
        setup_account::<T>(&maker, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        // Deep enough that every action in the batch can fill.
        Pallet::<T>::place_order(
            RawOrigin::Signed(maker).into(), 0,
            resting_request::<T>(1, Side::Sell, 50_000, (a as Lots).saturating_mul(10)),
        )
        .expect("the resting sell must place");
        let a = a.min(T::MaxActionsPerBatch::get());
        let mut actions = alloc::vec::Vec::new();
        for i in 0..a {
            actions.push(ExchangeAction::Place(resting_request::<T>(
                i as ClientOrderId + 2,
                Side::Buy,
                50_000,
                10,
            )));
        }
        let actions: BoundedVec<_, T::MaxActionsPerBatch> = BoundedVec::truncate_from(actions);

        #[extrinsic_call]
        execute_batch(RawOrigin::Signed(caller), 0, actions);
    }

    /// `set_liquidity_tier` (S1-4): a flat call that iterates every market on the tier.
    ///
    /// It used to borrow `create_market()`'s weight — a single market write — so the number of
    /// markets refreshed was invisible to the block budget.
    #[benchmark]
    fn set_liquidity_tier(m: Linear<1, { T::MaxMarkets::get() }>) {
        let tier = LiquidityTier {
            name: *b"bench   ",
            initial_margin_ppm: 100_000,
            maintenance_margin_ppm: 50_000,
            impact_notional_quote: 500_000_000,
        };
        crate::LiquidityTiers::<T>::insert(0u32, tier);
        for id in 0..m {
            Markets::<T>::insert(
                id as MarketId,
                bench_market_with_tier::<T>(id as MarketId, 0),
            );
        }
        NextMarketId::<T>::put(m as MarketId);

        #[extrinsic_call]
        set_liquidity_tier(RawOrigin::Root, 0u32, tier);
    }

    #[benchmark]
    fn prune_orders(r: Linear<1, 16>) {
        let caller: T::AccountId = whitelisted_caller();
        let maker: T::AccountId = account("maker", 0, 0);
        install_market::<T>();
        setup_account::<T>(&maker, DEPOSIT);
        setup_account::<T>(&caller, DEPOSIT);
        let r = r
            .min(T::MaxPrunePerCall::get())
            .min(T::MaxOrdersPerLevel::get());
        for i in 0..r {
            Pallet::<T>::place_order(
                RawOrigin::Signed(maker.clone()).into(),
                resting_request::<T>(i as ClientOrderId + 1, Side::Buy, 59_000 - i as u64, 10),
            )
            .unwrap();
        }
        // Expire everything the maker just placed.
        Markets::<T>::mutate(BENCH_MARKET, |maybe| {
            if let Some(market) = maybe {
            }
        });
        crate::Orders::<T>::iter_prefix(BENCH_MARKET).for_each(|(id, mut order)| {
            order.expires_at = frame_system::Pallet::<T>::block_number();
            crate::Orders::<T>::insert(BENCH_MARKET, id, order);
        });

        #[extrinsic_call]
        prune_orders(RawOrigin::Signed(caller), BENCH_MARKET, Side::Buy, r);
    }

    #[benchmark]
    fn create_market() -> Result<(), BenchmarkError> {
        NextMarketId::<T>::put(BENCH_MARKET);
        let config = MarketConfig {
            base_asset_code: *b"BTC     ",
            quote_asset_code: *b"USD     ",
            tick_size_quote: TICK,
            lot_size_base: LOT,
            base_precision: PRECISION,
            liquidity_tier: 0,
            maker_fee_ppm: 100,
            taker_fee_ppm: 500,
            max_position_lots: 10_000_000,
        };
        let origin =
            T::ForceOrigin::try_successful_origin().map_err(|_| BenchmarkError::Stop("origin"))?;

        #[block]
        {
            Pallet::<T>::create_market(origin, config)
                .map_err(|_| BenchmarkError::Stop("create_market"))?;
        }

        assert!(Markets::<T>::contains_key(BENCH_MARKET));
        Ok(())
    }

    #[benchmark]
    fn set_market_status() -> Result<(), BenchmarkError> {
        install_market::<T>();
        let origin =
            T::ForceOrigin::try_successful_origin().map_err(|_| BenchmarkError::Stop("origin"))?;

        #[block]
        {
            Pallet::<T>::set_market_status(origin, BENCH_MARKET, MarketStatus::ReduceOnly)
                .map_err(|_| BenchmarkError::Stop("set_market_status"))?;
        }

        Ok(())
    }

    #[benchmark]
    fn on_initialize() {
        install_market::<T>();
        let tick = T::FundingTickPeriod::get();

        // Fill the sample buffer so the tick has a full history to summarise.
        PremiumSamples::<T>::try_mutate_exists(
            BENCH_MARKET,
            |maybe| -> Result<(), BenchmarkError> {
                let mut samples: BoundedVec<i32, T::MaxPremiumSamples> = maybe
                    .take()
                    .unwrap_or_else(|| BoundedVec::try_from(alloc::vec![]).unwrap());
                while (samples.len() as u32) < T::MaxPremiumSamples::get() {
                    samples
                        .try_push(1_000)
                        .map_err(|_| BenchmarkError::Stop("premium samples"))?;
                }
                *maybe = Some(samples);
                Ok(())
            },
        )
        .unwrap();

        frame_system::Pallet::<T>::set_block_number(tick);

        #[block]
        {
            Pallet::<T>::on_initialize(tick);
        }
    }

    #[benchmark]
    fn set_risk_params() -> Result<(), BenchmarkError> {
        install_market::<T>();
        let origin =
            T::ForceOrigin::try_successful_origin().map_err(|_| BenchmarkError::Stop("origin"))?;
        let params = RiskParams {
            initial_margin_ppm: 50_000,
            maintenance_margin_ppm: 25_000,
            maker_fee_ppm: 100,
            taker_fee_ppm: 500,
            max_position_lots: 10_000_000,
        };

        #[block]
        {
            Pallet::<T>::set_risk_params(origin, BENCH_MARKET, params)
                .map_err(|_| BenchmarkError::Stop("set_risk_params"))?;
        }

        Ok(())
    }

    // Runs every benchmark above as a plain test, so a broken setup (an account that is not
    // actually liquidatable, a market that never got installed, ...) fails here instead of
    // surfacing as `BenchmarkError::Stop` during a long CLI run.
    impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
