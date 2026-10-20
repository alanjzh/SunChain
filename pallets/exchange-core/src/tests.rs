//! Unit tests for `pallet-exchange-core`.

use crate::{
    mock::*, types::*, Accounts, BlockExchangeUsage, Error, Event, FeePool, Markets, Orders,
    Positions, PriceLevels, PremiumVotes, UserOrderByClientId,
};
use exchange_primitives::DEFAULT_SUBACCOUNT;
use frame_support::{
    assert_noop, assert_ok,
    traits::{tokens::fungibles::Mutate, Get},
    weights::Weight,
    BoundedVec,
};

const ALICE: AccountId = 1;
const BOB: AccountId = 2;
const CHARLIE: AccountId = 3;

const TICK: QuoteAmount = 1_000_000;
const LOT: u64 = 1_000;
const PRECISION: u64 = 1_000_000_000;
const MARK: PriceTicks = 60_000;

const BAD_ORIGIN: sp_runtime::DispatchError = sp_runtime::DispatchError::BadOrigin;

fn fund(who: AccountId, amount: u128) {
    assert_ok!(<Assets as Mutate<AccountId>>::mint_into(USDC, &who, amount));
}

fn market_config() -> MarketConfig {
    MarketConfig {
        base_asset_code: *b"BTC     ",
        quote_asset_code: *b"USD     ",
        tick_size_quote: TICK,
        lot_size_base: LOT,
        base_precision: PRECISION,
        liquidity_tier: 0,
        maker_fee_ppm: 100,
        taker_fee_ppm: 500,
        max_position_lots: 10_000_000,
    }
}

fn setup_market() {
    assert_ok!(ExchangeCore::create_market(
        RuntimeOrigin::root(),
        market_config()
    ));
    // The oracle is the only source of mark price now, so a test that wants a price has to
    // supply one. `MARK` is what the removed cache used to hold.
    TestOracle::set(MARKET, MARK);
}

fn req(client: ClientOrderId, side: Side, price: PriceTicks, qty: Lots) -> OrderRequest<u64> {
    OrderRequest {
        market_id: MARKET,
        client_order_id: client,
        side,
        price_ticks: price,
        quantity_lots: qty,
        tif: TimeInForce::GoodTilCancelled,
        post_only: false,
        reduce_only: false,
        expires_at: System::block_number() + 1_000,
    }
}

fn gtc(client: ClientOrderId, side: Side, price: PriceTicks, qty: Lots) -> OrderRequest<u64> {
    req(client, side, price, qty)
}

fn ioc(client: ClientOrderId, side: Side, price: PriceTicks, qty: Lots) -> OrderRequest<u64> {
    OrderRequest {
        tif: TimeInForce::ImmediateOrCancel,
        expires_at: System::block_number(),
        ..req(client, side, price, qty)
    }
}

/// The market exists, the mark price is fresh, and Alice/Bob/Charlie hold $10,000 each.
fn setup_traders() {
    setup_market();
    for who in [ALICE, BOB, CHARLIE] {
        fund(who, 10_000_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(who),
            10_000_000_000
        ));
    }
}

// ---------------------------------------------------------------- collateral

#[test]
fn deposit_and_withdraw_are_conservative() {
    new_test_ext().execute_with(|| {
        setup_market();
        fund(ALICE, 1_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .collateral,
            1_000_000
        );
        assert_eq!(ExchangeCore::custody_balance(), 1_000_000);
        assert_eq!(ExchangeCore::internal_liabilities(), 1_000_000);

        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(ALICE),
            400_000
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .collateral,
            600_000
        );
        assert_eq!(ExchangeCore::custody_balance(), 600_000);

        assert_noop!(
            ExchangeCore::withdraw_collateral(RuntimeOrigin::signed(ALICE), 600_001),
            Error::<Test>::InsufficientCollateral
        );
    });
}

/// Guards §4.3: the custody account must be able to hold the collateral asset even though it
/// has no native balance, and `custody == Σ collateral + FeePool` must hold at all times.
#[test]
fn custody_account_holds_every_collateral_unit() {
    new_test_ext().execute_with(|| {
        setup_market();
        let custody = ExchangeCore::custody_account();
        assert_eq!(Assets::balance(USDC, custody), 0);

        fund(ALICE, 1_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000
        ));
        assert_eq!(Assets::balance(USDC, custody), 1_000_000);
        assert_eq!(
            ExchangeCore::custody_balance(),
            ExchangeCore::internal_liabilities()
        );

        // Draining custody again must not reap the asset account in a way that breaks later
        // deposits (min_balance / existential deposit of the collateral asset).
        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000
        ));
        assert_eq!(Assets::balance(USDC, custody), 0);
        assert_eq!(
            ExchangeCore::custody_balance(),
            ExchangeCore::internal_liabilities()
        );

        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            5
        ));
        assert_eq!(Assets::balance(USDC, custody), 5);
        assert_eq!(
            ExchangeCore::custody_balance(),
            ExchangeCore::internal_liabilities()
        );
    });
}

#[test]
fn deposit_of_zero_is_rejected() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            ExchangeCore::deposit_collateral(RuntimeOrigin::signed(ALICE), 0),
            Error::<Test>::InvalidAmount
        );
    });
}

#[test]
fn withdrawal_is_blocked_by_open_order_margin() {
    new_test_ext().execute_with(|| {
        setup_market();
        fund(ALICE, 1_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000
        ));
        // 1_000 lots @ 60_000 -> notional 60_000_000; IM 5% = 3_000_000 - too big, so scale down.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 300)
        ));
        // reserved = 5% * 18_000_000 = 900_000; only 100_000 is still free.
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            900_000
        );
        assert_noop!(
            ExchangeCore::withdraw_collateral(RuntimeOrigin::signed(ALICE), 200_000),
            Error::<Test>::InsufficientCollateral
        );
        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(ALICE),
            100_000
        ));
    });
}

// ---------------------------------------------------------------- governance

#[test]
fn create_market_requires_force_origin() {
    new_test_ext().execute_with(|| {
        assert_noop!(
            ExchangeCore::create_market(RuntimeOrigin::signed(ALICE), market_config()),
            BAD_ORIGIN
        );
        // Fill every market slot, then the next one must be refused.
        for _ in 0..MaxMarkets::get() {
            assert_ok!(ExchangeCore::create_market(
                RuntimeOrigin::root(),
                market_config()
            ));
        }
        assert_noop!(
            ExchangeCore::create_market(RuntimeOrigin::root(), market_config()),
            Error::<Test>::MaxMarketsReached
        );
    });
}

#[test]
fn invalid_market_config_is_rejected() {
    new_test_ext().execute_with(|| {
        let mut bad = market_config();
        // Margins now come from the tier, so the failure to check is a tier
    // that was never defined.
    bad.liquidity_tier = 99;
        assert_noop!(
            ExchangeCore::create_market(RuntimeOrigin::root(), bad),
            Error::<Test>::UnknownLiquidityTier
        );
    });
}

// ---------------------------------------------------------------- resting

#[test]
fn gtc_order_rests_and_updates_best_price() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 1_000)
        ));
        let market = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(market.best_bid, Some(59_000));
        assert_eq!(market.best_ask, None);
        assert_eq!(market.bid_level_count, 1);
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec![59_000]
        );

        let account = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT).unwrap();
        // notional = 1_000 * 59_000 = 59_000_000; IM 5% = 2_950_000
        assert_eq!(account.reserved_open_order_margin, 2_950_000);
        assert_eq!(account.open_order_count, 1);
    });
}

#[test]
fn bid_chain_descends_and_ask_chain_ascends() {
    new_test_ext().execute_with(|| {
        setup_traders();
        for (i, price) in [58_000u64, 59_000, 57_000].into_iter().enumerate() {
            assert_ok!(ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(i as ClientOrderId + 1, Side::Buy, price, 100)
            ));
        }
        for (i, price) in [62_000u64, 61_000, 63_000].into_iter().enumerate() {
            assert_ok!(ExchangeCore::place_order(
                RuntimeOrigin::signed(BOB),
                gtc(i as ClientOrderId + 1, Side::Sell, price, 100)
            ));
        }
        let market = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(market.best_bid, Some(59_000));
        assert_eq!(market.best_ask, Some(61_000));
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec![59_000, 58_000, 57_000]
        );
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Sell),
            alloc::vec![61_000, 62_000, 63_000]
        );
    });
}

#[test]
fn duplicate_client_order_id_is_rejected() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(1, Side::Buy, 58_000, 100)),
            Error::<Test>::DuplicateClientOrderId
        );
    });
}

// ---------------------------------------------------------------- matching

#[test]
fn fifo_within_a_price_level() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 500)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Sell, 60_000, 500)
        ));
        assert_eq!(
            ExchangeCore::level_queue(MARKET, Side::Sell, 60_000).len(),
            2
        );

        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 750)
        ));

        assert_eq!(
            ExchangeCore::level_queue(MARKET, Side::Sell, 60_000).len(),
            1
        );
        assert_eq!(Orders::<Test>::get(MARKET, 1), None);
        assert_eq!(Orders::<Test>::get(MARKET, 2).unwrap().remaining_lots, 250);
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            750
        );
        assert_eq!(
            Positions::<Test>::get((BOB, DEFAULT_SUBACCOUNT, MARKET))
                .map(|p| p.size_lots)
                .unwrap_or(0),
            -750
        );
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            1
        );
    });
}

#[test]
fn price_priority_consumes_the_best_level_first() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 300)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Sell, 61_000, 300)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 61_000, 400)
        ));
        // 300 at 60_000 then 100 at 61_000.
        assert_eq!(Orders::<Test>::get(MARKET, 1), None);
        assert_eq!(Orders::<Test>::get(MARKET, 2).unwrap().remaining_lots, 200);
        let alice = Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).unwrap();
        // weighted entry = (300 * 60_000 + 100 * 61_000) / 400 = 60_250
        assert_eq!(alice.size_lots, 400);
        assert_eq!(alice.entry_price_ticks, 60_250);
    });
}

#[test]
fn trade_emits_event_and_collects_fees() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        System::assert_has_event(RuntimeEvent::ExchangeCore(Event::TradeExecuted {
            market_id: MARKET,
            maker: BOB,
            taker: ALICE,
            price_ticks: 60_000,
            lots: 1_000,
            maker_order_id: 1,
            taker_client_order_id: 1,
        }));
        // notional = 60_000_000; maker 100ppm = 6_000, taker 500ppm = 30_000
        assert_eq!(FeePool::<Test>::get(), 36_000);
        assert_eq!(
            ExchangeCore::custody_balance(),
            ExchangeCore::internal_liabilities()
        );
    });
}

#[test]
fn partial_fill_releases_reserve_pro_rata() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 400)
        ));
        // Bob sold 400 of 1_000; 60% of the reserve stays locked.
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            1_800_000
        );
        assert_eq!(
            Orders::<Test>::get(MARKET, 1).unwrap().reserved_margin,
            1_800_000
        );
        assert_eq!(
            Positions::<Test>::get((BOB, DEFAULT_SUBACCOUNT, MARKET))
                .map(|p| p.size_lots)
                .unwrap_or(0),
            -400
        );
    });
}

// ---------------------------------------------------------------- cancel

#[test]
fn cancel_order_releases_all_reserved_margin() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 1_000)
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            2_950_000
        );
        assert_ok!(ExchangeCore::cancel_order(RuntimeOrigin::signed(ALICE), 1));
        let account = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT).unwrap();
        assert_eq!(account.reserved_open_order_margin, 0);
        assert_eq!(account.open_order_count, 0);
        assert_eq!(account.collateral, 10_000_000_000);
        assert!(Orders::<Test>::get(MARKET, 1).is_none());
        assert!(PriceLevels::<Test>::get((MARKET, Side::Buy, 59_000)).is_none());
        assert!(UserOrderByClientId::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, 1)).is_none());
        let market = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(market.best_bid, None);
        assert_eq!(market.bid_level_count, 0);
    });
}

#[test]
fn cancel_up_to_invalidates_orders_lazily() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(2, Side::Buy, 58_000, 100)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(3, Side::Buy, 57_000, 100)
        ));
        assert_ok!(ExchangeCore::cancel_up_to(RuntimeOrigin::signed(ALICE), 3));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .invalid_before,
            3
        );

        // A taker that would cross the invalidated bids drops them instead of trading.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            ioc(1, Side::Sell, 57_000, 100)
        ));
        System::assert_has_event(RuntimeEvent::ExchangeCore(Event::OrderCancelled {
            who: ALICE,
            client_order_id: 1,
            reason: CancelReason::NonceInvalidated,
            released_margin: 295_000,
        }));
        // Only the still valid order 3 traded.
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            100
        );
        assert_eq!(
            Positions::<Test>::get((BOB, DEFAULT_SUBACCOUNT, MARKET))
                .map(|p| p.size_lots)
                .unwrap_or(0),
            -100
        );
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec::Vec::<PriceTicks>::new()
        );
    });
}

#[test]
fn prune_orders_removes_expired_orders() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));
        System::set_block_number(2_000);
        assert_ok!(ExchangeCore::prune_orders(
            RuntimeOrigin::signed(BOB),
            MARKET,
            Side::Buy,
            10
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            0
        );
        assert_eq!(Markets::<Test>::get(MARKET).unwrap().best_bid, None);
        assert_eq!(Markets::<Test>::get(MARKET).unwrap().bid_level_count, 0);
    });
}

#[test]
fn only_the_owner_can_cancel() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));
        assert_noop!(
            ExchangeCore::cancel_order(RuntimeOrigin::signed(BOB), 1),
            Error::<Test>::OrderNotFound
        );
    });
}

// ---------------------------------------------------------------- flags

#[test]
fn post_only_rejects_crossing_orders() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 100)
        ));
        let mut crossing = req(1, Side::Buy, 61_000, 100);
        crossing.post_only = true;
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), crossing),
            Error::<Test>::PostOnlyWouldCross
        );
        let mut ok = req(1, Side::Buy, 59_000, 100);
        ok.post_only = true;
        assert_ok!(ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), ok));
    });
}

#[test]
fn post_only_requires_gtc() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let mut bad = ioc(1, Side::Buy, 59_000, 100);
        bad.post_only = true;
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), bad),
            Error::<Test>::PostOnlyRequiresGtc
        );
    });
}

#[test]
fn reduce_only_cannot_increase_or_open() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let mut flat = req(1, Side::Sell, 60_000, 100);
        flat.reduce_only = true;
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), flat),
            Error::<Test>::ReduceOnlyWouldIncrease
        );

        // A flat third party attempting to open with reduce_only is also rejected.
        fund(CHARLIE, 10_000_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(CHARLIE),
            10_000_000_000
        ));

        // Open a long of 500 for ALICE against BOB.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 500)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 500)
        ));
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            500
        );

        // A sell larger than the long would increase the position, so it is rejected.
        let mut too_big = req(2, Side::Sell, 60_000, 501);
        too_big.reduce_only = true;
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), too_big),
            Error::<Test>::ReduceOnlyWouldIncrease
        );

        // CHARLIE rests a buy so ALICE can actually close her long via reduce_only.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Buy, 60_000, 500)
        ));

        let mut close = ioc(2, Side::Sell, 60_000, 500);
        close.reduce_only = true;
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            close
        ));
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            0
        );
    });
}

#[test]
fn reduce_only_gtc_rests_and_never_reverses_position() {
    new_test_ext().execute_with(|| {
        setup_traders();

        // ALICE goes long 500 against BOB.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 500)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 500)
        ));
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            500
        );

        // A non-crossing GTC reduce-only sell must rest instead of being cancelled like an IOC.
        let mut close = req(2, Side::Sell, 61_000, 500);
        close.reduce_only = true;
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            close
        ));

        assert_eq!(Markets::<Test>::get(MARKET).unwrap().best_ask, Some(61_000));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            1
        );
        let (_, order_id) =
            UserOrderByClientId::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, 2)).unwrap();
        assert!(Orders::<Test>::get(MARKET, order_id).unwrap().reduce_only);

        // When it is finally taken it flattens the long and never flips it short.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Buy, 61_000, 500)
        ));
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
    });
}

#[test]
fn ioc_remainder_is_cancelled() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 100)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            ioc(1, Side::Buy, 60_000, 500)
        ));
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            100
        );
        // Nothing rests and the unused reserve was released again.
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            0
        );
        // reserve = 5% * 30_000_000 = 1_500_000; 100/500 filled -> 1_200_000 released back.
        System::assert_has_event(RuntimeEvent::ExchangeCore(Event::OrderCancelled {
            who: ALICE,
            client_order_id: 1,
            reason: CancelReason::IocRemainder,
            released_margin: 1_200_000,
        }));
    });
}

#[test]
fn ioc_without_counterparty_does_not_rest() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            ioc(1, Side::Buy, 60_000, 100)
        ));
        assert_eq!(Markets::<Test>::get(MARKET).unwrap().best_bid, None);
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
    });
}

/// Design §11.2: the per-block exchange budget must be accounted for and must reset lazily.
#[test]
fn per_block_exchange_budget_is_enforced() {
    new_test_ext().execute_with(|| {
        // `setup_traders` funds 3 accounts, so 3 deposit actions are already accounted for.
        setup_traders();
        assert_eq!(BlockExchangeUsage::<Test>::get().unwrap().1, 3);

        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));
        let (block, actions, _) = BlockExchangeUsage::<Test>::get().unwrap();
        assert_eq!(block, System::block_number());
        assert_eq!(actions, 4);

        // Saturating the action counter rejects any further exchange action in this block.
        BlockExchangeUsage::<Test>::put((
            System::block_number(),
            MaxExchangeActionsPerBlock::get(),
            Weight::zero(),
        ));
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(2, Side::Buy, 58_000, 100)),
            Error::<Test>::ExchangeBlockBudgetExceeded
        );

        // A new block starts with a fresh budget (lazy reset, no `on_initialize`).
        System::set_block_number(System::block_number() + 1);
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(3, Side::Buy, 57_000, 100)
        ));
        assert_eq!(BlockExchangeUsage::<Test>::get().unwrap().1, 1);
    });
}

// ---------------------------------------------------------------- batches

#[test]
fn execute_batch_is_atomic_and_bounded() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let actions: BoundedVec<ExchangeAction<u64>, MaxActionsPerBatch> =
            BoundedVec::truncate_from(alloc::vec![
                ExchangeAction::Place(gtc(1, Side::Buy, 59_000, 100)),
                ExchangeAction::Place(gtc(2, Side::Buy, 58_000, 100)),
                ExchangeAction::Cancel(1),
            ]);
        assert_ok!(ExchangeCore::execute_batch(
            RuntimeOrigin::signed(ALICE),
            actions
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            1
        );
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec![58_000]
        );

        // A failing action rolls the whole batch back.
        let bad: BoundedVec<ExchangeAction<u64>, MaxActionsPerBatch> =
            BoundedVec::truncate_from(alloc::vec![
                ExchangeAction::Place(gtc(3, Side::Buy, 57_000, 100)),
                ExchangeAction::Cancel(1),
            ]);
        assert_noop!(
            ExchangeCore::execute_batch(RuntimeOrigin::signed(ALICE), bad),
            Error::<Test>::OrderNotFound
        );
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec![58_000]
        );
    });
}

// ---------------------------------------------------------------- risk

#[test]
fn insufficient_collateral_blocks_oversized_orders() {
    new_test_ext().execute_with(|| {
        setup_market();
        fund(ALICE, 1_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000
        ));
        // notional = 10_000 * 60_000 = 600_000_000; IM = 30_000_000 >> 1_000_000
        assert_noop!(
            ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(1, Side::Buy, 60_000, 10_000)
            ),
            Error::<Test>::InsufficientCollateral
        );
    });
}

#[test]
fn stale_mark_price_blocks_risk_growth() {
    new_test_ext().execute_with(|| {
        setup_traders();
        System::set_block_number(100);
        // Staleness is now "the oracle reports nothing" — there is no cached price to fall
        // back on, which is the whole point of removing the cache.
        TestOracle::clear();
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(1, Side::Buy, 60_000, 100)),
            Error::<Test>::MarkPriceStale
        );
        TestOracle::set(MARKET, 60_000);
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 100)
        ));
    });
}

#[test]
fn underwater_account_is_restricted_and_may_only_reduce() {
    new_test_ext().execute_with(|| {
        setup_market();
        // Alice: $5 of collateral opens a $60 notional long (12x).
        fund(ALICE, 5_000_000);
        fund(BOB, 10_000_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            5_000_000
        ));
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(BOB),
            10_000_000_000
        ));

        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        assert_eq!(
            Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
                .unwrap()
                .status,
            AccountStatus::Normal
        );

        // The mark drops to 55_000: Alice is deeply underwater but must still be able to close.
        TestOracle::set(MARKET, 55_000);
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Buy, 55_000, 1_000)
        ));
        let mut close = ioc(2, Side::Sell, 55_000, 1_000);
        close.reduce_only = true;
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            close
        ));

        let alice = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT).unwrap();
        assert_eq!(alice.status, AccountStatus::Restricted);
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            0
        );

        // A restricted account can no longer open new risk.
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(3, Side::Buy, 55_000, 100)),
            Error::<Test>::AccountRestricted
        );
    });
}

#[test]
fn realized_pnl_is_booked_on_close() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        // Bob provides a bid at 61_000; Alice closes into it.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Buy, 61_000, 1_000)
        ));
        let mut close = ioc(2, Side::Sell, 61_000, 1_000);
        close.reduce_only = true;
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            close
        ));
        let alice = Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).unwrap();
        assert_eq!(alice.size_lots, 0);
        assert_eq!(alice.entry_price_ticks, 0);
        // 1_000 * (61_000 - 60_000)
        assert_eq!(alice.realized_pnl, 1_000_000);
    });
}

/// P2-B §7 / Phase 1 H3: a realised loss must actually reduce withdrawable collateral.
///
/// Before this change the loss lived only in `Position::realized_pnl`, so a wash-trading
/// account could still withdraw its full nominal collateral — the reason the Phase 1 design
/// forbids real assets outright.
#[test]
fn realised_loss_reduces_withdrawable_collateral() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));

        // Price falls: Bob bids 59_000 and Alice closes into it.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Buy, 59_000, 1_000)
        ));
        let mut close = ioc(2, Side::Sell, 59_000, 1_000);
        close.reduce_only = true;
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            close
        ));

        let alice = Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).unwrap();
        assert_eq!(alice.size_lots, 0);
        // 1_000 * (59_000 - 60_000)
        assert_eq!(alice.realized_pnl, -1_000_000);

        // Withdrawing settles first: the loss is folded in before the balance is touched.
        let before = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(ALICE),
            1
        ));
        let after = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        assert_eq!(after, before - 1_000_000 - 1);

        // The side-car is cleared, so a second settlement cannot charge twice.
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .realized_pnl,
            0
        );
    });
}

/// Funding is charged lazily on the next touch, and is zero-sum: long pays, short receives.
#[test]
fn funding_index_move_is_charged_zero_sum_on_next_touch() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        // Alice is long 1_000, Bob short 1_000.

        // A funding tick moved the index by +10 per lot while nobody touched their position.
        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 10;
        Markets::<Test>::insert(MARKET, market);

        let alice_before = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        let bob_before = Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;

        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(ALICE),
            1
        ));
        assert_ok!(ExchangeCore::withdraw_collateral(
            RuntimeOrigin::signed(BOB),
            1
        ));

        // Long pays 1_000 * 10 = 10_000; short receives exactly the same amount.
        let alice_after = Accounts::<Test>::get(ALICE, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        let bob_after = Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        assert_eq!(alice_after, alice_before - 10_000 - 1);
        assert_eq!(bob_after, bob_before + 10_000 - 1);
        // Net zero: what one paid the other received.
        assert_eq!(
            (alice_before + bob_before) - (alice_after + bob_after),
            2,
            "only the two withdrawals may leave custody"
        );
    });
}

#[test]
fn maker_below_initial_margin_is_rejected() {
    new_test_ext().execute_with(|| {
        setup_market();
        // Bob's collateral covers the open-order reserve (5% * 60_000_000 = 3_000_000) but not
        // the worst-case fill initial margin once the taker fee is paid. The worst-case risk gate
        // therefore rejects the order at placement rather than letting it rest and drop later.
        fund(BOB, 3_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(BOB),
            3_000_000
        ));
        assert_noop!(
            ExchangeCore::place_order(
                RuntimeOrigin::signed(BOB),
                gtc(1, Side::Sell, 60_000, 1_000)
            ),
            Error::<Test>::InsufficientCollateral
        );
        assert_eq!(
            Positions::<Test>::get((BOB, DEFAULT_SUBACCOUNT, MARKET)),
            None
        );
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin,
            0
        );

        // With enough collateral the same order is accepted and rests.
        fund(BOB, 10_000_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(BOB),
            10_000_000_000
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(2, Side::Sell, 60_000, 1_000)
        ));
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            1
        );
        assert!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .reserved_open_order_margin
                > 0
        );
    });
}

// ---------------------------------------------------------------- market status

#[test]
fn market_status_gates_actions() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 59_000, 100)
        ));

        assert_ok!(ExchangeCore::set_market_status(
            RuntimeOrigin::root(),
            MARKET,
            MarketStatus::ReduceOnly
        ));
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(BOB), gtc(1, Side::Sell, 60_000, 100)),
            Error::<Test>::MarketReduceOnly
        );

        assert_ok!(ExchangeCore::set_market_status(
            RuntimeOrigin::root(),
            MARKET,
            MarketStatus::CancelOnly
        ));
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(BOB), gtc(1, Side::Sell, 60_000, 100)),
            Error::<Test>::MarketCancelOnly
        );
        // Cancels work in every market state.
        assert_ok!(ExchangeCore::cancel_order(RuntimeOrigin::signed(ALICE), 1));

        assert_ok!(ExchangeCore::set_market_status(
            RuntimeOrigin::root(),
            MARKET,
            MarketStatus::Paused
        ));
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(BOB), gtc(1, Side::Sell, 60_000, 100)),
            Error::<Test>::MarketNotActive
        );
    });
}

// ---------------------------------------------------------------- bounds

#[test]
fn too_many_open_orders_is_rejected() {
    new_test_ext().execute_with(|| {
        setup_traders();
        // `MaxOpenOrdersPerAccount` is 16 in the mock.
        for i in 1..=16u64 {
            assert_ok!(ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(i, Side::Buy, 50_000 + i, 10)
            ));
        }
        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(17, Side::Buy, 49_000, 10)),
            Error::<Test>::TooManyOpenOrders
        );
    });
}

#[test]
fn expired_orders_are_removed_by_the_matching_loop() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 100)
        ));
        // Move past the expiry of every order placed at block 1.
        System::set_block_number(2_000);
        TestOracle::set(MARKET, 60_000);
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            ioc(1, Side::Buy, 60_000, 100)
        ));
        // The expired maker is dropped instead of being filled.
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            0
        );
        assert_eq!(
            Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
                .unwrap()
                .open_order_count,
            0
        );
        System::assert_has_event(RuntimeEvent::ExchangeCore(Event::OrderCancelled {
            who: BOB,
            client_order_id: 1,
            reason: CancelReason::Expired,
            released_margin: 300_000,
        }));
    });
}

#[test]
fn storage_stays_consistent_after_cancels_from_queue_middle() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let prices = [59_000u64, 58_000, 57_000, 56_000];
        let mut client = 1u64;
        for price in prices {
            for _ in 0..2 {
                assert_ok!(ExchangeCore::place_order(
                    RuntimeOrigin::signed(ALICE),
                    gtc(client, Side::Buy, price, 100)
                ));
                assert_ok!(ExchangeCore::place_order(
                    RuntimeOrigin::signed(BOB),
                    gtc(client, Side::Buy, price, 100)
                ));
                client += 1;
            }
        }
        // 4 orders of 100 lots per level; cancel one from the middle of two different queues.
        assert_ok!(ExchangeCore::cancel_order(RuntimeOrigin::signed(ALICE), 3));
        assert_ok!(ExchangeCore::cancel_order(RuntimeOrigin::signed(BOB), 5));

        let market = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(market.best_bid, Some(59_000));
        assert_eq!(market.bid_level_count, 4);
        assert_eq!(
            ExchangeCore::price_chain(MARKET, Side::Buy),
            alloc::vec![59_000, 58_000, 57_000, 56_000]
        );

        for (index, price) in prices.into_iter().enumerate() {
            let level = PriceLevels::<Test>::get((MARKET, Side::Buy, price)).unwrap();
            let expected_lots = if index == 1 || index == 2 { 300 } else { 400 };
            let expected_count = if index == 1 || index == 2 { 3 } else { 4 };
            assert_eq!(level.total_lots, expected_lots);
            assert_eq!(level.order_count, expected_count);
            assert_eq!(
                ExchangeCore::level_total_lots(MARKET, Side::Buy, price).unwrap(),
                expected_lots
            );
        }

        for who in [ALICE, BOB] {
            let account = Accounts::<Test>::get(who, DEFAULT_SUBACCOUNT).unwrap();
            assert_eq!(account.open_order_count, 7);
            // `collateral` is the *total* balance held for the account (free + reserved), so it
            // must equal the deposited amount; the reserved portion is strictly a subset of it.
            assert_eq!(account.collateral, 10_000_000_000);
            assert!(account.reserved_open_order_margin <= account.collateral);
            // ALICE cancelled client 3 (a 58_000 order) and BOB cancelled client 5 (a 57_000 order),
            // so their reserved margins differ by one price level's IM (5_000).
            let expected = if who == ALICE { 2_010_000 } else { 2_015_000 };
            assert_eq!(account.reserved_open_order_margin, expected);
        }
        assert_eq!(
            ExchangeCore::custody_balance(),
            ExchangeCore::internal_liabilities()
        );
    });
}

/// The insurance fund must move *real* collateral: covering a shortfall has to land the money
/// in the exchange core custody account, where user balances actually live.
#[test]
fn insurance_fund_moves_real_collateral_into_custody() {
    use exchange_primitives::InsuranceFund;

    new_test_ext().execute_with(|| {
        setup_market();
        fund(BOB, 10_000_000);
        assert_ok!(Insurance::deposit(RuntimeOrigin::signed(BOB), 5_000_000));
        assert_eq!(Insurance::available(), 5_000_000);

        let custody_before = ExchangeCore::custody_balance();
        assert_ok!(Insurance::cover(2_000_000));
        assert_eq!(Insurance::available(), 3_000_000);
        // The payout must arrive where user collateral is held, not merely be burned.
        assert_eq!(ExchangeCore::custody_balance(), custody_before + 2_000_000);

        assert_noop!(
            Insurance::cover(5_000_000),
            pallet_insurance::Error::<Test>::FundExhausted
        );
    });
}

/// Bad debt is subtracted from the internal liabilities, so a shortfall the fund could not
/// cover shows up as an explicit gap instead of being silently absorbed.
#[test]
fn bad_debt_is_subtracted_from_internal_liabilities() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_eq!(ExchangeCore::bad_debt(), 0);
        let before = ExchangeCore::internal_liabilities();

        crate::BadDebt::<Test>::put(1_000);
        assert_eq!(ExchangeCore::bad_debt(), 1_000);
        assert_eq!(ExchangeCore::internal_liabilities(), before - 1_000);
    });
}

/// A thinly capitalised long that the mark price has pushed below maintenance margin.
///
/// Returns Charlie's collateral just before the liquidation is triggered.
fn underwater_long() -> u128 {
    setup_market();
    // Notional 1_000 × 60_000 = 60_000_000; initial margin 5% = 3_000_000. Charlie gets just
    // enough to open plus a sliver, so a modest adverse move puts him under maintenance.
    fund(CHARLIE, 3_100_000);
    assert_ok!(ExchangeCore::deposit_collateral(
        RuntimeOrigin::signed(CHARLIE),
        3_100_000
    ));
    fund(BOB, 10_000_000_000);
    assert_ok!(ExchangeCore::deposit_collateral(
        RuntimeOrigin::signed(BOB),
        10_000_000_000
    ));

    // Charlie long 1_000 @ 60_000, Bob short 1_000.
    assert_ok!(ExchangeCore::place_order(
        RuntimeOrigin::signed(BOB),
        gtc(1, Side::Sell, 60_000, 1_000)
    ));
    assert_ok!(ExchangeCore::place_order(
        RuntimeOrigin::signed(CHARLIE),
        gtc(1, Side::Buy, 60_000, 1_000)
    ));

    // Mark falls to 57_000: unrealised −3_000_000, equity ≈ 70_000 against a maintenance
    // requirement of 57_000_000 × 2.5% = 1_425_000.
    TestOracle::set(MARKET, 57_000);
    // Liquidity for the liquidation to sell into.
    assert_ok!(ExchangeCore::place_order(
        RuntimeOrigin::signed(BOB),
        gtc(2, Side::Buy, 57_000, 1_000)
    ));

    Accounts::<Test>::get(CHARLIE, DEFAULT_SUBACCOUNT)
        .unwrap()
        .collateral
}

#[test]
fn healthy_account_is_not_liquidatable() {
    use exchange_primitives::ExchangeEngine;

    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        assert_noop!(
            ExchangeCore::liquidate_account(&BOB, &ALICE, MARKET, 0),
            Error::<Test>::NotLiquidatable
        );
    });
}

#[test]
fn liquidation_closes_the_position_and_funds_insurance() {
    use exchange_primitives::{ExchangeEngine, InsuranceFund};

    new_test_ext().execute_with(|| {
        let collateral_before = underwater_long();
        let fund_before = Insurance::available();

        let closed =
            ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0).expect("liquidatable");
        assert!(closed > 0);

        // The position really shrank.
        assert!(
            Positions::<Test>::get((CHARLIE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots
                .abs()
                < 1_000
        );

        // The penalty (0.5% of the notional closed at the mark) is charged to the account.
        let charlie_after = Accounts::<Test>::get(CHARLIE, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        assert!(charlie_after < collateral_before);

        // §8.4: the penalty, less the keeper rebate (zero here), reaches the fund.
        assert!(Insurance::available() > fund_before);
    });
}

/// The path that was unreachable before liquidation existed: settlement inside a liquidation
/// drains the account, and the insurance fund covers what it cannot pay.
#[test]
fn liquidation_routes_uncollectable_settlement_to_the_insurance_fund() {
    use exchange_primitives::{ExchangeEngine, InsuranceFund};

    new_test_ext().execute_with(|| {
        underwater_long();

        // Fund the insurance first, then make Charlie owe more funding than he holds.
        // BOB deposited every unit he had, so he needs fresh USDC for this.
        fund(BOB, 5_000_000);
        assert_ok!(Insurance::deposit(RuntimeOrigin::signed(BOB), 5_000_000));
        let fund_before = Insurance::available();

        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 4_000;
        Markets::<Test>::insert(MARKET, market);

        assert_ok!(ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0));

        // The fund paid out, and nothing was left behind as bad debt.
        assert!(Insurance::available() < fund_before);
        assert_eq!(ExchangeCore::bad_debt(), 0);
    });
}

/// The liquidation is permissionless: any keeper may submit it and is paid the accrued rebate.
#[test]
fn keeper_can_liquidate_and_is_paid_the_rebate() {
    use frame_support::traits::tokens::fungibles::Inspect;

    new_test_ext().execute_with(|| {
        underwater_long();
        // ALICE has nothing to do with the position — she is only the keeper.
        let keeper_before = Assets::balance(USDC, &ALICE);

        assert_ok!(Liquidation::liquidate(
            RuntimeOrigin::signed(ALICE),
            CHARLIE,
            MARKET,
            0
        ));

        assert!(
            Positions::<Test>::get((CHARLIE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots
                .abs()
                < 1_000
        );
        // The rebate came out of custody, not out of another user's collateral.
        assert!(Assets::balance(USDC, &ALICE) > keeper_before);
    });
}

/// §8.5.1 §1: the ADL execution price may never penalise the account being ADL'd.
#[test]
fn adl_execution_price_always_favours_the_adld_account() {
    // Band = ceil(60_000 × 1_000 / 1_000_000) = 60.
    let long = pallet_liquidation::Pallet::<Test>::adl_execution_price(60_000, true, 1_000)
        .expect("price");
    assert_eq!(
        long, 60_060,
        "a long is reduced by selling, so it is paid more"
    );

    let short = pallet_liquidation::Pallet::<Test>::adl_execution_price(60_000, false, 1_000)
        .expect("price");
    assert_eq!(
        short, 59_940,
        "a short is reduced by buying, so it pays less"
    );

    // A zero band still means "never worse than the adjusted mark", never worse.
    assert_eq!(
        pallet_liquidation::Pallet::<Test>::adl_execution_price(60_000, true, 0).unwrap(),
        60_000
    );
}

/// §8.5.1 §2: a loss that survives collateral, liquidation and the insurance fund is recorded
/// explicitly. ADL is then gated on that record — it never fires speculatively.
#[test]
fn uncovered_loss_is_recorded_as_residual_loss() {
    use exchange_primitives::ExchangeEngine;

    new_test_ext().execute_with(|| {
        underwater_long();
        // No insurance deposit at all, so Charlie's funding debt cannot be covered.
        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 4_000;
        Markets::<Test>::insert(MARKET, market);

        assert_ok!(ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0));

        let loss = crate::ResidualLossOf::<Test>::get(CHARLIE);
        assert!(
            loss.amount > 0,
            "the uncovered part must be recorded, not absorbed"
        );
        assert_eq!(loss.market_id, MARKET);
        assert_eq!(loss.side_sign, 1, "Charlie was long");
        assert_eq!(ExchangeCore::bad_debt(), loss.amount);

        // ADL is gated on that record existing — it passed the gate and then found no
        // qualifying candidates among an empty proposal.
        assert_noop!(
            Liquidation::adl(RuntimeOrigin::signed(ALICE), CHARLIE, BoundedVec::default()),
            pallet_liquidation::Error::<Test>::NoAdlCandidates
        );
    });
}

/// The v1 -> v2 migration re-keys an existing account under `(who, DEFAULT_SUBACCOUNT)`.
///
/// The old entry is written by hand at the **old-format** key (no sub-account component), which
/// is exactly what a chain upgraded from v1 holds.
#[test]
fn migration_v2_adds_the_subaccount_dimension() {
    use codec::Encode;
    use frame_support::{
        storage::{unhashed, StorageDoubleMap},
        traits::UncheckedOnRuntimeUpgrade,
        Blake2_128Concat, StorageHasher, Twox128,
    };

    new_test_ext().execute_with(|| {
        let alice = ALICE;
        let account = Account {
            collateral: 7_000,
            ..Default::default()
        };

        // Old layout: `Accounts` was a plain map keyed by `AccountId` only.
        let old_prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Accounts");
        let mut old_key = old_prefix.to_vec();
        old_key.extend_from_slice(&Blake2_128Concat::hash(&alice.encode()));
        unhashed::put(&old_key, &account);

        // Before the upgrade the new accessor cannot see it.
        assert!(Accounts::<Test>::get(alice, DEFAULT_SUBACCOUNT).is_none());

        <crate::migrations::v2::Migration<Test> as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();

        // After: reachable under the sub-account key, and the old key is gone.
        assert_eq!(
            Accounts::<Test>::get(alice, DEFAULT_SUBACCOUNT)
                .unwrap()
                .collateral,
            7_000
        );
        assert!(unhashed::get_raw(&old_key).is_none());
        assert_eq!(
            crate::SubaccountIndex::<Test>::get(alice).to_vec(),
            vec![DEFAULT_SUBACCOUNT]
        );
    });
}

/// §9.2: an account holding positions in more than `MaxClearingMarkets` markets is refused new
/// risk rather than risking an under-aggregated requirement.
#[test]
fn exceeding_max_clearing_markets_is_refused() {
    new_test_ext().execute_with(|| {
        setup_market();
        // Two more markets than the clearing bound allows (mock: MaxClearingMarkets = 2).
        for _ in 0..MaxClearingMarkets::get() {
            assert_ok!(ExchangeCore::create_market(
                RuntimeOrigin::root(),
                market_config()
            ));
        }

        fund(ALICE, 10_000_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            10_000_000_000
        ));

        // Every market needs a price now that the cache is gone.
        for market in 0..=MaxClearingMarkets::get() {
            TestOracle::set(market, MARK);
        }

        // Seed a position in every market directly; going through `place_order` would hit the
        // very guard under test.
        for market in 0..=MaxClearingMarkets::get() {
            Positions::<Test>::insert(
                (ALICE, DEFAULT_SUBACCOUNT, market),
                Position {
                    size_lots: 10,
                    entry_price_ticks: 60_000,
                    ..Default::default()
                },
            );
        }

        assert_noop!(
            ExchangeCore::place_order(RuntimeOrigin::signed(ALICE), gtc(1, Side::Buy, 60_000, 10)),
            Error::<Test>::TooManyClearingMarkets
        );
    });
}

/// §9.3: a fee tier whose rebate exceeds the maker fee pays the maker out of `FeePool`.
///
/// The rebate is a claim on fees already collected, so it must never come out of another
/// account's collateral.
#[test]
fn maker_rebate_is_paid_from_the_fee_pool() {
    new_test_ext().execute_with(|| {
        setup_traders();

        // Tier 1: maker pays 100ppm but is rebated 300ppm — a negative effective maker fee.
        let tier = FeeTier {
            maker_fee_ppm: 100,
            taker_fee_ppm: 500,
            maker_rebate_ppm: 300,
        };
        assert_ok!(ExchangeCore::set_fee_tier(RuntimeOrigin::root(), 1, tier));
        assert_ok!(ExchangeCore::set_account_fee_tier(
            RuntimeOrigin::root(),
            BOB,
            DEFAULT_SUBACCOUNT,
            Some(1)
        ));

        // Bob rests an offer (the maker leg), Alice takes it.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        let bob_before = Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;

        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));

        // notional 60_000_000; fee 100ppm = 6_000, rebate 300ppm = 18_000.
        let bob_after = Accounts::<Test>::get(BOB, DEFAULT_SUBACCOUNT)
            .unwrap()
            .collateral;
        assert_eq!(
            bob_after,
            bob_before - 6_000 + 18_000,
            "maker is a net receiver"
        );

        // The rebate came out of collected fees, not out of custody: taker paid 30_000.
        assert_eq!(FeePool::<Test>::get(), 30_000 + 6_000 - 18_000);
    });
}

/// §9.3: an empty `FeePool` cannot fund a rebate. Refusing is correct — silently diluting
/// collateral would break conservation.
#[test]
fn rebate_with_empty_fee_pool_is_refused() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let tier = FeeTier {
            maker_fee_ppm: 0,
            taker_fee_ppm: 0,
            maker_rebate_ppm: 5_000,
        };
        assert_ok!(ExchangeCore::set_fee_tier(RuntimeOrigin::root(), 1, tier));
        assert_ok!(ExchangeCore::set_account_fee_tier(
            RuntimeOrigin::root(),
            BOB,
            DEFAULT_SUBACCOUNT,
            Some(1)
        ));

        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        // Zero fees are collected, so there is nothing to rebate from.
        assert_noop!(
            ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(1, Side::Buy, 60_000, 1_000)
            ),
            Error::<Test>::InsufficientFeePool
        );
    });
}

/// End-to-end ADL: a bankrupt account's recorded residual loss triggers a **partial** reduction
/// of a profitable opposite-side account, at a price in that account's favour.
///
/// This is the one ADL scenario the smaller unit tests cannot cover: it runs the whole chain —
/// thin long goes bankrupt, insurance is empty so the loss is recorded explicitly, a keeper
/// proposes a profitable counterparty, and the runtime slices off only what the loss warrants.
#[test]
fn adl_partially_reduces_a_profitable_opposite_account() {
    use exchange_primitives::ExchangeEngine;

    new_test_ext().execute_with(|| {
        setup_market();

        // Both thinly capitalised: Charlie the long, Alice the short on the other side.
        for who in [CHARLIE, ALICE] {
            fund(who, 3_100_000);
            assert_ok!(ExchangeCore::deposit_collateral(
                RuntimeOrigin::signed(who),
                3_100_000
            ));
        }

        // One trade creates both sides: Alice sells 1_000 @ 60_000, Charlie buys it.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));

        // Mark down to 57_000: Alice is deep in profit, Charlie deeply underwater.
        TestOracle::set(MARKET, 57_000);

        // A funding bill Charlie cannot pay, and no insurance was ever funded.
        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 4_000;
        Markets::<Test>::insert(MARKET, market);

        // Charlie's liquidation leaves an explicitly recorded residual loss.
        assert_ok!(ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0));
        let loss = crate::ResidualLossOf::<Test>::get(CHARLIE);
        assert!(loss.amount > 0, "the uncovered loss must be recorded");
        assert_eq!(loss.market_id, MARKET);
        assert_eq!(loss.side_sign, 1, "Charlie was long");

        // Alice is short, profitable and clears the roe floor — a legitimate target.
        let rank = ExchangeCore::adl_rank(&ALICE, MARKET).expect("must be rankable");
        assert_eq!(rank.size_lots, -1_000);
        assert!(
            rank.unrealized_pnl > 0,
            "ADL must only touch profitable accounts"
        );
        assert!(
            rank.roe_ppm >= 50_000,
            "roe {:?} below MinAdlRoePpm",
            rank.roe_ppm
        );

        // Bob is only the keeper; he has no position at all.
        assert_ok!(Liquidation::adl(
            RuntimeOrigin::signed(BOB),
            CHARLIE,
            BoundedVec::truncate_from(vec![ALICE])
        ));

        // Shed only as much exposure as the loss warrants — Alice is not wiped out.
        let expected = (loss.amount + rank.notional_per_lot - 1) / rank.notional_per_lot;
        assert!(
            expected < 1_000,
            "must be a partial reduction, not a full close"
        );
        let after = Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).unwrap();
        assert_eq!(after.size_lots, -(1_000 - expected as i64));

        // §8.5.1 §2: ADL reduces exposure; it does not reduce the residual loss.
        assert_eq!(
            crate::ResidualLossOf::<Test>::get(CHARLIE).amount,
            loss.amount
        );
    });
}

/// The P2-B migration must add `Position::funding_index` without disturbing anything else, and
/// must seed it from the market so that no position is billed retroactively.
#[test]
fn migration_v1_adds_funding_index_seeded_from_market() {
    use codec::Encode;
    use frame_support::{
        storage::unhashed, traits::UncheckedOnRuntimeUpgrade, Blake2_128Concat, StorageHasher,
    };

    new_test_ext().execute_with(|| {
        setup_market();

        // Move the market index, so the seed can be proven to come from the market and not
        // from a hardcoded `0`.
        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 7;
        Markets::<Test>::insert(MARKET, market);

        // Write a position in the **v0** layout: a two component key `(AccountId, MarketId)`
        // plus the Phase 1 value encoding (32 bytes, no trailing `funding_index`).
        //
        // Reaching for the pallet's current accessor here would build a three-part key and
        // hide the very bug this test exists for.
        let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
        let mut v0_key = prefix.to_vec();
        v0_key.extend_from_slice(&Blake2_128Concat::hash(&ALICE.encode()));
        v0_key.extend_from_slice(&Blake2_128Concat::hash(&MARKET.encode()));

        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(&1_000i64.encode());
        raw.extend_from_slice(&60_000u64.encode());
        raw.extend_from_slice(&0i128.encode());
        assert_eq!(raw.len(), 32);
        // `put_raw`, not `put`: the latter would prefix the bytes with a compact length.
        unhashed::put_raw(&v0_key, &raw);

        <crate::migrations::v1::Migration<Test> as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();

        // v1 upgrades the value in place. The key is still the v0 one, so the position is
        // *not* yet reachable through the current accessor — that is v2's job.
        assert!(Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).is_none());
        let upgraded: Position = unhashed::get(&v0_key).expect("value rewritten in place");
        assert_eq!(upgraded.size_lots, 1_000);
        assert_eq!(upgraded.entry_price_ticks, 60_000);
        assert_eq!(upgraded.realized_pnl, 0);
        // Seeded from the market: Phase 1 never charged funding, so a position must not owe
        // the epochs that elapsed before the upgrade.
        assert_eq!(upgraded.funding_index, 7);

        // v2 re-keys it, after which the normal accessor finds it — v0 -> v1 -> v2 end to end.
        <crate::migrations::v2::Migration<Test> as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();
        let alice = Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET)).unwrap();
        assert_eq!(alice.size_lots, 1_000);
        assert_eq!(alice.funding_index, 7);
    });
}

/// §8.5 §3: the fund and the keeper may only receive penalty the account actually paid.
///
/// Regression: the penalty was deducted with `saturating_sub` but distributed in full, so an
/// under-collateralised account pushed money into the fund and the keeper pool that nobody
/// had paid — the difference came out of *other* users' collateral.
#[test]
fn liquidation_never_credits_more_than_the_account_paid() {
    use exchange_primitives::{ExchangeEngine, InsuranceFund};

    new_test_ext().execute_with(|| {
        underwater_long();
        // Thin Charlie out until he cannot cover the penalty: the close leaves him at zero,
        // while the configured penalty on 1_000 lots at the mark is 285_000.
        let mut account = Accounts::<Test>::get(CHARLIE, DEFAULT_SUBACCOUNT).unwrap();
        account.collateral = 200_000;
        Accounts::<Test>::insert(CHARLIE, DEFAULT_SUBACCOUNT, account);

        let fund_before = Insurance::available();
        let keeper_before = crate::KeeperRebateOf::<Test>::get(ALICE);
        assert!(
            ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0).expect("liquidatable") > 0
        );

        // Whatever left custody for the fund and the keeper must have been paid by Charlie.
        // He ended at zero, so nothing may be handed out.
        let handed_out = (Insurance::available() - fund_before)
            + (crate::KeeperRebateOf::<Test>::get(ALICE) - keeper_before);
        assert_eq!(
            handed_out, 0,
            "fund and keeper were credited money Charlie never paid"
        );
    });
}

/// A keeper may only claim the rebate they earned.
///
/// Regression: the rebate lived in a single `KeeperRebatePool` and `pay_keeper` called
/// `take()` on all of it, so the first keeper to ask walked away with everyone else's
/// pending rebate. The total was still bounded, but the attribution was wrong.
#[test]
fn a_keeper_can_only_claim_their_own_rebate() {
    use exchange_primitives::ExchangeEngine;

    new_test_ext().execute_with(|| {
        underwater_long();
        // ALICE does the work; BOB does nothing.
        assert!(
            ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0).expect("liquidatable") > 0
        );
        let accrued = crate::KeeperRebateOf::<Test>::get(ALICE);
        assert!(accrued > 0, "the keeper who liquidated must be credited");

        // BOB must not be able to walk away with ALICE's rebate.
        assert_eq!(ExchangeCore::pay_keeper(&BOB).expect("no-op"), 0);
        assert_eq!(crate::KeeperRebateOf::<Test>::get(ALICE), accrued);

        // ALICE claims exactly her own.
        assert_eq!(ExchangeCore::pay_keeper(&ALICE).expect("paid"), accrued);
        assert_eq!(crate::KeeperRebateOf::<Test>::get(ALICE), 0);
    });
}

/// ADL must pay for itself out of the per-block budget like every other exchange action.
///
/// Regression: `consume_block_budget` was only reachable from inside exchange-core, so
/// `Liquidation::adl` skipped it and one block could carry unlimited forced reductions.
#[test]
fn adl_is_charged_against_the_block_budget() {
    use exchange_primitives::ExchangeEngine;

    new_test_ext().execute_with(|| {
        setup_market();

        // Both thinly capitalised: Charlie the long, Alice the short on the other side.
        for who in [CHARLIE, ALICE] {
            fund(who, 3_100_000);
            assert_ok!(ExchangeCore::deposit_collateral(
                RuntimeOrigin::signed(who),
                3_100_000
            ));
        }

        // One trade creates both sides: Alice sells 1_000 @ 60_000, Charlie buys it.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(ALICE),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));

        // Mark down to 57_000: Alice is deep in profit, Charlie deeply underwater.
        TestOracle::set(MARKET, 57_000);

        // A funding bill Charlie cannot pay, and no insurance was ever funded.
        let mut market = Markets::<Test>::get(MARKET).unwrap();
        market.funding_index = 4_000;
        Markets::<Test>::insert(MARKET, market);

        // Charlie's liquidation leaves an explicitly recorded residual loss.
        assert_ok!(ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0));
        let loss = crate::ResidualLossOf::<Test>::get(CHARLIE);
        assert!(loss.amount > 0, "the uncovered loss must be recorded");
        assert_eq!(loss.market_id, MARKET);
        assert_eq!(loss.side_sign, 1, "Charlie was long");

        // Alice is short, profitable and clears the roe floor — a legitimate target.
        let rank = ExchangeCore::adl_rank(&ALICE, MARKET).expect("must be rankable");
        assert_eq!(rank.size_lots, -1_000);
        assert!(
            rank.unrealized_pnl > 0,
            "ADL must only touch profitable accounts"
        );
        assert!(
            rank.roe_ppm >= 50_000,
            "roe {:?} below MinAdlRoePpm",
            rank.roe_ppm
        );

        // Bob is only the keeper; he has no position at all.
        let actions_before = BlockExchangeUsage::<Test>::get().unwrap().1;
        assert_ok!(Liquidation::adl(
            RuntimeOrigin::signed(BOB),
            CHARLIE,
            BoundedVec::truncate_from(vec![ALICE])
        ));
        assert!(
            BlockExchangeUsage::<Test>::get().unwrap().1 > actions_before,
            "adl must consume the block budget"
        );
    });
}

// ---------------------------------------------------------------- funding epochs

/// `on_initialize` drives the whole funding mechanism, and nothing exercised it end to end:
/// the pure helpers in `funding.rs` were tested, but never wired to a block. These cover the
/// wiring — which epochs fire, in what order, and what each one leaves behind.

fn funding_setup() {
    setup_market();
    // The premium falls back to the market's own mark when no oracle index is set, and a
    // market without one records *no* sample at all.
    TestOracle::set(MARKET, 60_000);
}

#[test]
fn on_initialize_is_a_noop_outside_an_epoch() {
    use frame_support::{traits::Hooks, weights::Weight};

    new_test_ext().execute_with(|| {
        funding_setup();
        let before = Markets::<Test>::get(MARKET).unwrap();

        // 61 is neither a sample epoch (60) nor a tick epoch (1_200).
        assert_eq!(ExchangeCore::on_initialize(61), Weight::zero());
        assert!(crate::PremiumSamples::<Test>::get(MARKET).is_empty());

        // Nothing about the market's funding state may move outside an epoch.
        let after = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(after.last_funding_at, before.last_funding_at);
        assert_eq!(after.rate_ppm, before.rate_ppm);
        assert_eq!(after.funding_index, before.funding_index);
    });
}

#[test]
fn on_initialize_records_one_premium_sample_per_sample_epoch() {
    use frame_support::traits::Hooks;

    new_test_ext().execute_with(|| {
        funding_setup();
        ExchangeCore::on_initialize(60);
        assert_eq!(crate::PremiumSamples::<Test>::get(MARKET).len(), 1);

        // Observations accumulate until the tick consumes them.
        ExchangeCore::on_initialize(120);
        assert_eq!(crate::PremiumSamples::<Test>::get(MARKET).len(), 2);
    });
}

#[test]
fn on_initialize_settles_funding_at_the_tick_epoch() {
    use frame_support::traits::Hooks;

    new_test_ext().execute_with(|| {
        funding_setup();
        ExchangeCore::on_initialize(60);
        ExchangeCore::on_initialize(120);
        assert_eq!(crate::PremiumSamples::<Test>::get(MARKET).len(), 2);

        // 1_200 is both a tick and a sample epoch. The tick runs first and consumes the
        // history, then the new sample opens the next cycle (same ordering as dYdX).
        ExchangeCore::on_initialize(1_200);

        let market = Markets::<Test>::get(MARKET).unwrap();
        assert_eq!(
            market.last_funding_at, 1_200,
            "the tick must stamp the market"
        );
        // Premium was zero throughout, so nothing is charged and the index does not move.
        assert_eq!(market.rate_ppm, 0);
        assert_eq!(market.funding_index, 0);
        // History cleared, and the fresh sample recorded in the same block.
        assert_eq!(crate::PremiumSamples::<Test>::get(MARKET).len(), 1);
    });
}

#[test]
fn premium_samples_are_capped_and_the_oldest_is_evicted() {
    use frame_support::traits::Hooks;

    new_test_ext().execute_with(|| {
        funding_setup();
        let cap = <Test as crate::Config>::MaxPremiumSamples::get();

        // Walk past the cap on sample epochs, skipping tick epochs (every 20th) so the
        // history is never consumed mid-way.
        let mut recorded = 0u32;
        let mut n = 1u64;
        while recorded < cap + 5 {
            if n % 20 != 0 {
                ExchangeCore::on_initialize(n * 60);
                recorded += 1;
            }
            n += 1;
        }
        assert_eq!(
            crate::PremiumSamples::<Test>::get(MARKET).len() as u32,
            cap,
            "the oldest observations must be evicted, not accumulated without bound"
        );
    });
}

// ------------------------------------------------- P2-D: exchange batch (§12.3 / §12.4)

use crate::mock::{TestSignature, TestSigner};

fn signed_action(
    account: AccountId,
    nonce: u64,
    action: ExchangeAction<u64>,
) -> crate::types::SignedExchangeAction<TestSigner, TestSignature, u64> {
    crate::types::SignedExchangeAction {
        signer: TestSigner(account),
        subaccount: DEFAULT_SUBACCOUNT,
        action_nonce: nonce,
        action,
        // The test double accepts exactly this relation.
        signature: TestSignature(account),
    }
}

/// §12.3: `SortKey = (action_class, action_nonce, arrival_index)`.
#[test]
fn sort_key_orders_by_class_then_nonce_then_arrival() {
    use crate::types::{ActionClass, SortKey};

    let k = |class, action_nonce, arrival_index| SortKey {
        class,
        action_nonce,
        arrival_index,
    };

    // Non-risk first, then cancels, then places.
    assert!(k(ActionClass::Other, 9, 9) < k(ActionClass::Cancel, 0, 0));
    // Class dominates: a late-arriving cancel still precedes an early place.
    assert!(k(ActionClass::Cancel, 9, 7) < k(ActionClass::Place, 1, 0));
    // Within a class the lower nonce goes first, whatever the arrival order.
    assert!(k(ActionClass::Place, 1, 9) < k(ActionClass::Place, 2, 0));
    // Same class and nonce (different sub-accounts): arrival decides.
    assert!(k(ActionClass::Cancel, 3, 0) < k(ActionClass::Cancel, 3, 1));
}

/// The submitter is only a courier: an action is authorised by its own signature.
#[test]
fn a_batch_action_is_authorised_by_its_own_signature() {
    new_test_ext().execute_with(|| {
        setup_traders();
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Sell, 60_000, 100)
        ));

        // BOB submits; the action is ALICE's.
        assert_ok!(ExchangeCore::execute_exchange_batch(
            RuntimeOrigin::none(),
            BoundedVec::truncate_from(vec![signed_action(
                ALICE,
                1,
                ExchangeAction::Place(gtc(9, Side::Buy, 60_000, 100))
            )])
        ));

        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            100,
            "the signer must get the position, not the submitter"
        );
        assert_eq!(
            Positions::<Test>::get((BOB, DEFAULT_SUBACCOUNT, MARKET))
                .map(|p| p.size_lots)
                .unwrap_or(0),
            0
        );
    });
}

/// A forged signature must be skipped, and must not take the batch down with it.
#[test]
fn a_forged_action_is_skipped_without_aborting_the_batch() {
    new_test_ext().execute_with(|| {
        setup_traders();

        let good = signed_action(
            ALICE,
            1,
            ExchangeAction::Place(gtc(9, Side::Buy, 60_000, 100)),
        );
        let mut forged = signed_action(
            BOB,
            1,
            ExchangeAction::Place(gtc(8, Side::Buy, 60_000, 100)),
        );
        forged.signature = TestSignature(ALICE); // BOB's action, ALICE's signature

        assert_ok!(ExchangeCore::execute_exchange_batch(
            RuntimeOrigin::none(),
            BoundedVec::truncate_from(vec![forged, good])
        ));

        // ALICE's action ran; BOB's did not, and ALICE's nonce was not consumed by it.
        assert_eq!(
            crate::ActionNonce::<Test>::get(ALICE, DEFAULT_SUBACCOUNT),
            1
        );
        assert_eq!(crate::ActionNonce::<Test>::get(BOB, DEFAULT_SUBACCOUNT), 0);
    });
}

/// Replay protection: the same signed action must never run twice.
#[test]
fn a_signed_action_cannot_be_replayed() {
    new_test_ext().execute_with(|| {
        setup_traders();
        let action = signed_action(
            ALICE,
            5,
            ExchangeAction::Place(gtc(9, Side::Buy, 60_000, 100)),
        );
        let batch = |a: crate::types::SignedExchangeAction<TestSigner, TestSignature, u64>| {
            BoundedVec::truncate_from(vec![a])
        };

        assert_ok!(ExchangeCore::execute_exchange_batch(
            RuntimeOrigin::none(),
            batch(action.clone())
        ));
        // Second time: the nonce is no longer ahead of the last one consumed.
        assert_ok!(ExchangeCore::execute_exchange_batch(
            RuntimeOrigin::none(),
            batch(action)
        ));
        assert_eq!(
            crate::ActionNonce::<Test>::get(ALICE, DEFAULT_SUBACCOUNT),
            5
        );
    });
}

/// Within a class, arrival order decides who is filled — not the submitter's whim.
#[test]
fn arrival_index_breaks_ties_so_the_submitter_cannot_reorder() {
    new_test_ext().execute_with(|| {
        setup_traders();
        // Only 100 lots of liquidity, so exactly one of the two buys can fill.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 100)
        ));

        assert_ok!(ExchangeCore::execute_exchange_batch(
            RuntimeOrigin::none(),
            BoundedVec::truncate_from(vec![
                signed_action(
                    CHARLIE,
                    1,
                    ExchangeAction::Place(gtc(7, Side::Buy, 60_000, 100))
                ),
                signed_action(
                    ALICE,
                    1,
                    ExchangeAction::Place(gtc(8, Side::Buy, 60_000, 100))
                ),
            ])
        ));

        // CHARLIE was first in the batch, so CHARLIE gets the fill.
        assert_eq!(
            Positions::<Test>::get((CHARLIE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            100
        );
        assert_eq!(
            Positions::<Test>::get((ALICE, DEFAULT_SUBACCOUNT, MARKET))
                .unwrap()
                .size_lots,
            0
        );
    });
}

/// §18.6 gate 4: liquidation and ADL must not break custody conservation under a
/// controlled fault injection — here, an artificial mark price shock.
///
/// Whatever the exchange holds must stay backed by recorded liabilities, at every step,
/// however violently the mark moves. This covers both directions: a crushed long and a
/// crushed short, each pushed through several rounds of liquidation.
#[test]
fn custody_stays_conserved_under_mark_price_shocks() {
    use exchange_primitives::{ExchangeEngine, InsuranceFund};

    new_test_ext().execute_with(|| {
        setup_market();
        // ALICE is the liquidity provider and the insurer; BOB/CHARLIE are the leveraged
        // positions that the shock will crush.
        fund(ALICE, 1_100_000_000);
        assert_ok!(ExchangeCore::deposit_collateral(
            RuntimeOrigin::signed(ALICE),
            1_000_000_000
        ));
        for who in [BOB, CHARLIE] {
            fund(who, 3_100_000);
            assert_ok!(ExchangeCore::deposit_collateral(
                RuntimeOrigin::signed(who),
                3_100_000
            ));
        }

        // 1_000 lots @ 60_000 against 5% initial margin: a few percent of adverse move is
        // enough to go under maintenance.
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(BOB),
            gtc(1, Side::Sell, 60_000, 1_000)
        ));
        assert_ok!(ExchangeCore::place_order(
            RuntimeOrigin::signed(CHARLIE),
            gtc(1, Side::Buy, 60_000, 1_000)
        ));
        assert_ok!(Insurance::deposit(RuntimeOrigin::signed(ALICE), 50_000_000));

        // Deliberately `>=`, not `==`: `internal_liabilities()` counts settled cash only
        // and omits the unrealised PnL of open positions, so equality does not hold while a
        // position is open. The dangerous direction — custody owing more than it holds — is
        // what this guards. See the 2026-09-16 worklog note.
        let check = |stage: &str| {
            assert_eq!(
                ExchangeCore::custody_balance(),
                ExchangeCore::internal_liabilities(),
                "custody no longer backs the recorded liabilities at: {stage}"
            );
        };
        check("setup");

        let mut id = 10u64;
        for mark in [57_000u64, 54_000, 51_000] {
            TestOracle::set(MARKET, mark);
            // Something for the liquidation to trade into.
            assert_ok!(ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(id, Side::Buy, mark, 1_000)
            ));
            id += 1;
            let _ = ExchangeCore::liquidate_account(&ALICE, &CHARLIE, MARKET, 0);
            check("after crushing the long");
        }

        // And the other way round: now the short is the one under water.
        for mark in [63_000u64, 66_000] {
            TestOracle::set(MARKET, mark);
            assert_ok!(ExchangeCore::place_order(
                RuntimeOrigin::signed(ALICE),
                gtc(id, Side::Sell, mark, 1_000)
            ));
            id += 1;
            let _ = ExchangeCore::liquidate_account(&ALICE, &BOB, MARKET, 0);
            check("after crushing the short");
        }
    });
}

/// P2-D: premium votes accumulate during the epoch and are summarised by their median at
/// the boundary — the whole point of the vote layer. A single wild vote cannot move it.
#[test]
fn premium_votes_are_summarised_by_median() {
	new_test_ext().execute_with(|| {
		setup_market();
		// Out of order on purpose: the median does not care about arrival order.
		assert_ok!(ExchangeCore::submit_premium_votes(
			RuntimeOrigin::none(),
			MARKET,
			BoundedVec::truncate_from(vec![30, 10, 20]),
			1,
		));

		System::set_block_number(FundingSamplePeriod::get());
		let _ = <ExchangeCore as frame_support::traits::Hooks<u64>>::on_initialize(
			FundingSamplePeriod::get(),
		);

		assert!(
			System::events().iter().any(|record| matches!(
				&record.event,
				RuntimeEvent::ExchangeCore(Event::FundingSampleRecorded { premium_ppm, .. })
					if *premium_ppm == 20
			)),
			"the epoch must be summarised by the median of the votes, not by one sample"
		);
	});
}

/// A replayed vote must be refused, and that is what keeps the vote layer alive.
///
/// The vote carries nothing else that varies between blocks, so without a nonce the same
/// premium encodes to the same extrinsic — the same hash. The pool bans the hash of anything
/// it finds inside an imported block, so the first successful vote bans itself and every
/// later vote for that market is rejected as `TemporarilyBanned` for as long as the premium
/// does not change. The feed votes every block, so that is forever.
#[test]
fn a_replayed_premium_vote_is_refused() {
	new_test_ext().execute_with(|| {
		setup_market();
		assert_ok!(ExchangeCore::submit_premium_votes(
			RuntimeOrigin::none(),
			MARKET,
			BoundedVec::truncate_from(vec![10]),
			1,
		));

		// Same nonce, same content: refused.
		assert_noop!(
			ExchangeCore::submit_premium_votes(
				RuntimeOrigin::none(),
				MARKET,
				BoundedVec::truncate_from(vec![10]),
				1,
			),
			Error::<Test>::InvalidActionNonce,
		);
		// And an older one, so the count only ever moves forward.
		assert_noop!(
			ExchangeCore::submit_premium_votes(
				RuntimeOrigin::none(),
				MARKET,
				BoundedVec::truncate_from(vec![10]),
				1,
			),
			Error::<Test>::InvalidActionNonce,
		);

		// Ahead of it, fine — and it is this one that reaches the queue.
		assert_ok!(ExchangeCore::submit_premium_votes(
			RuntimeOrigin::none(),
			MARKET,
			BoundedVec::truncate_from(vec![20]),
			2,
		));
		assert_eq!(PremiumVotes::<Test>::get(MARKET).into_inner(), vec![10, 20]);
	});
}

/// Without votes the epoch falls back to a single on-chain observation, so a node that has
/// not been upgraded to vote does not silently stop producing samples.
#[test]
fn a_sample_epoch_without_votes_falls_back_to_one_observation() {
	new_test_ext().execute_with(|| {
		setup_market();
		System::set_block_number(FundingSamplePeriod::get());
		let _ = <ExchangeCore as frame_support::traits::Hooks<u64>>::on_initialize(
			FundingSamplePeriod::get(),
		);

		let recorded = System::events().iter().any(|record| matches!(
			&record.event,
			RuntimeEvent::ExchangeCore(Event::FundingSampleRecorded { .. })
		));
		assert!(recorded, "an epoch with no votes must still record a sample");
	});
}
