//! Order book primitives: the price-level chain and the per-level FIFO queue.
//!
//! Every operation in this module is O(1) except [`Pallet::ensure_level`], which is bounded by
//! `MaxPriceLevelsPerSide`. No operation ever scans a `Vec` of order ids and no single storage
//! value can grow without bound.

use crate::{
    types::{
        Lots, Market, MarketId, Order, OrderId, PartyState, PriceLevel, PriceTicks, QuoteAmount,
        Side,
    },
    Accounts, Config, Error, Orders, Pallet, PriceLevels, UserOrderByClientId,
};
use exchange_primitives::DEFAULT_SUBACCOUNT;
use frame_support::{ensure, traits::Get};
use frame_system::pallet_prelude::BlockNumberFor;
use sp_runtime::{traits::Saturating, DispatchError};

impl<T: Config> Pallet<T> {
    /// Best resting price on `side`, or `None` when that side of the book is empty.
    pub(crate) fn best_price(market: &Market<BlockNumberFor<T>>, side: Side) -> Option<PriceTicks> {
        match side {
            Side::Buy => market.best_bid,
            Side::Sell => market.best_ask,
        }
    }

    /// Best price of the book side a taker order on `side` would consume.
    pub(crate) fn opposing_best_price(
        market: &Market<BlockNumberFor<T>>,
        side: Side,
    ) -> Option<PriceTicks> {
        Self::best_price(market, side.opposite())
    }

    /// Creates the price level for `price` when it does not exist yet and links it into the
    /// price chain. Returns the number of levels that had to be scanned; bounded by
    /// `MaxPriceLevelsPerSide`, otherwise `TooManyPriceLevels` is returned.
    pub(crate) fn ensure_level(
        market: &mut Market<BlockNumberFor<T>>,
        market_id: MarketId,
        side: Side,
        price: PriceTicks,
    ) -> Result<u32, DispatchError> {
        if PriceLevels::<T>::contains_key((market_id, side, price)) {
            return Ok(0);
        }

        let max_levels = T::MaxPriceLevelsPerSide::get();
        let count = match side {
            Side::Buy => market.bid_level_count,
            Side::Sell => market.ask_level_count,
        };
        ensure!(count < max_levels, Error::<T>::TooManyPriceLevels);

        let mut cur = Self::best_price(market, side);
        let mut prev: Option<PriceTicks> = None;
        let mut scanned: u32 = 0;

        // Bids descend from `best_bid`, asks ascend from `best_ask`.
        while let Some(p) = cur {
            let past_insertion_point = match side {
                Side::Buy => p < price,
                Side::Sell => p > price,
            };
            if past_insertion_point {
                break;
            }
            let level = PriceLevels::<T>::get((market_id, side, p))
                .ok_or(Error::<T>::InvariantViolation)?;
            prev = Some(p);
            cur = level.next_price;
            scanned = scanned.saturating_add(1);
            ensure!(scanned <= max_levels, Error::<T>::TooManyPriceLevels);
        }

        if let Some(p) = prev {
            let mut level = PriceLevels::<T>::get((market_id, side, p))
                .ok_or(Error::<T>::InvariantViolation)?;
            level.next_price = Some(price);
            PriceLevels::<T>::insert((market_id, side, p), level);
        }
        if let Some(n) = cur {
            let mut level = PriceLevels::<T>::get((market_id, side, n))
                .ok_or(Error::<T>::InvariantViolation)?;
            level.prev_price = Some(price);
            PriceLevels::<T>::insert((market_id, side, n), level);
        }

        PriceLevels::<T>::insert(
            (market_id, side, price),
            PriceLevel {
                head: None,
                tail: None,
                total_lots: 0,
                order_count: 0,
                prev_price: prev,
                next_price: cur,
            },
        );

        if prev.is_none() {
            match side {
                Side::Buy => market.best_bid = Some(price),
                Side::Sell => market.best_ask = Some(price),
            }
        }
        match side {
            Side::Buy => market.bid_level_count = market.bid_level_count.saturating_add(1),
            Side::Sell => market.ask_level_count = market.ask_level_count.saturating_add(1),
        }

        Ok(scanned)
    }

    /// Unlinks a (necessarily empty) price level from the price chain and removes it.
    fn remove_level(
        market: &mut Market<BlockNumberFor<T>>,
        market_id: MarketId,
        side: Side,
        price: PriceTicks,
    ) -> Result<(), DispatchError> {
        let level = PriceLevels::<T>::get((market_id, side, price))
            .ok_or(Error::<T>::InvariantViolation)?;

        match level.prev_price {
            Some(p) => {
                let mut prev = PriceLevels::<T>::get((market_id, side, p))
                    .ok_or(Error::<T>::InvariantViolation)?;
                prev.next_price = level.next_price;
                PriceLevels::<T>::insert((market_id, side, p), prev);
            }
            None => match side {
                Side::Buy => market.best_bid = level.next_price,
                Side::Sell => market.best_ask = level.next_price,
            },
        }
        if let Some(n) = level.next_price {
            let mut next = PriceLevels::<T>::get((market_id, side, n))
                .ok_or(Error::<T>::InvariantViolation)?;
            next.prev_price = level.prev_price;
            PriceLevels::<T>::insert((market_id, side, n), next);
        }

        match side {
            Side::Buy => market.bid_level_count = market.bid_level_count.saturating_sub(1),
            Side::Sell => market.ask_level_count = market.ask_level_count.saturating_sub(1),
        }
        PriceLevels::<T>::remove((market_id, side, price));
        Ok(())
    }

    /// Appends `order` to the tail of its price level FIFO queue.
    ///
    /// `order.remaining_lots` must already be set. Returns the number of price levels scanned.
    pub(crate) fn enqueue_order(
        market: &mut Market<BlockNumberFor<T>>,
        market_id: MarketId,
        order_id: OrderId,
        mut order: Order<T::AccountId, BlockNumberFor<T>>,
    ) -> Result<u32, DispatchError> {
        let side = order.side;
        let price = order.price_ticks;
        let scanned = Self::ensure_level(market, market_id, side, price)?;

        let mut level = PriceLevels::<T>::get((market_id, side, price))
            .ok_or(Error::<T>::InvariantViolation)?;
        ensure!(
            level.order_count < T::MaxOrdersPerLevel::get(),
            Error::<T>::TooManyOrdersAtLevel
        );

        order.prev_order = level.tail;
        order.next_order = None;
        match level.tail {
            Some(tail) => {
                let mut tail_order =
                    Orders::<T>::get(market_id, tail).ok_or(Error::<T>::InvariantViolation)?;
                tail_order.next_order = Some(order_id);
                Orders::<T>::insert(market_id, tail, tail_order);
            }
            None => level.head = Some(order_id),
        }
        level.tail = Some(order_id);
        level.total_lots = level
            .total_lots
            .checked_add(order.remaining_lots)
            .ok_or(Error::<T>::ArithmeticOverflow)?;
        level.order_count = level
            .order_count
            .checked_add(1)
            .ok_or(Error::<T>::ArithmeticOverflow)?;

        PriceLevels::<T>::insert((market_id, side, price), level);
        Orders::<T>::insert(market_id, order_id, order);
        Ok(scanned)
    }

    /// Detaches `order` from the book and releases its reserved margin.
    ///
    /// `taker_in_memory` must be `Some` when the order owner is the account whose state is
    /// currently held in memory by the matching loop; its reserve is then released from that
    /// copy instead of a second, stale storage read.
    ///
    /// Returns the amount of margin that was released.
    pub(crate) fn remove_order(
        market: &mut Market<BlockNumberFor<T>>,
        order_id: OrderId,
        order: &Order<T::AccountId, BlockNumberFor<T>>,
        taker_in_memory: Option<(&T::AccountId, &mut PartyState)>,
    ) -> Result<QuoteAmount, DispatchError> {
        let market_id = order.market_id;
        let side = order.side;
        let price = order.price_ticks;

        let mut level = PriceLevels::<T>::get((market_id, side, price))
            .ok_or(Error::<T>::InvariantViolation)?;

        match order.prev_order {
            Some(prev) => {
                let mut prev_order =
                    Orders::<T>::get(market_id, prev).ok_or(Error::<T>::InvariantViolation)?;
                prev_order.next_order = order.next_order;
                Orders::<T>::insert(market_id, prev, prev_order);
            }
            None => level.head = order.next_order,
        }
        match order.next_order {
            Some(next) => {
                let mut next_order =
                    Orders::<T>::get(market_id, next).ok_or(Error::<T>::InvariantViolation)?;
                next_order.prev_order = order.prev_order;
                Orders::<T>::insert(market_id, next, next_order);
            }
            None => level.tail = order.prev_order,
        }

        level.total_lots = level
            .total_lots
            .checked_sub(order.remaining_lots)
            .ok_or(Error::<T>::InvariantViolation)?;
        level.order_count = level
            .order_count
            .checked_sub(1)
            .ok_or(Error::<T>::InvariantViolation)?;

        Orders::<T>::remove(market_id, order_id);
        UserOrderByClientId::<T>::remove((&order.owner, DEFAULT_SUBACCOUNT, order.client_order_id));

        if level.order_count == 0 {
            Self::remove_level(market, market_id, side, price)?;
        } else {
            PriceLevels::<T>::insert((market_id, side, price), level);
        }

        Self::release_order_reserve(&order.owner, order.reserved_margin, taker_in_memory)
    }

    /// Credits `amount` of reserved margin back to `owner`, and decrements its open order count.
    pub(crate) fn release_order_reserve(
        owner: &T::AccountId,
        amount: QuoteAmount,
        taker_in_memory: Option<(&T::AccountId, &mut PartyState)>,
    ) -> Result<QuoteAmount, DispatchError> {
        match taker_in_memory {
            Some((taker, state)) if taker == owner => {
                state.account.reserved_open_order_margin = state
                    .account
                    .reserved_open_order_margin
                    .checked_sub(amount)
                    .ok_or(Error::<T>::InvariantViolation)?;
                state.account.open_order_count = state
                    .account
                    .open_order_count
                    .checked_sub(1)
                    .ok_or(Error::<T>::InvariantViolation)?;
            }
            _ => {
                Accounts::<T>::try_mutate(
                    owner,
                    DEFAULT_SUBACCOUNT,
                    |maybe| -> Result<(), DispatchError> {
                        let account = maybe.as_mut().ok_or(Error::<T>::InvariantViolation)?;
                        account.reserved_open_order_margin = account
                            .reserved_open_order_margin
                            .checked_sub(amount)
                            .ok_or(Error::<T>::InvariantViolation)?;
                        account.open_order_count = account
                            .open_order_count
                            .checked_sub(1)
                            .ok_or(Error::<T>::InvariantViolation)?;
                        Ok(())
                    },
                )?;
            }
        }
        Ok(amount)
    }

    /// Sum of `remaining_lots` of every order queued at `(market_id, side, price)`.
    ///
    /// Test/debug helper used by the invariant checks; never called on the hot path.
    #[allow(dead_code)]
    pub(crate) fn level_total_lots(
        market_id: MarketId,
        side: Side,
        price: PriceTicks,
    ) -> Result<Lots, DispatchError> {
        let level =
            PriceLevels::<T>::get((market_id, side, price)).ok_or(Error::<T>::OrderNotFound)?;
        let mut cursor = level.head;
        let mut total: Lots = 0;
        let mut guard = 0u32;
        while let Some(id) = cursor {
            let order = Orders::<T>::get(market_id, id).ok_or(Error::<T>::InvariantViolation)?;
            total = total.saturating_add(order.remaining_lots);
            cursor = order.next_order;
            guard = guard.saturating_add(1);
            ensure!(
                guard <= T::MaxOrdersPerLevel::get(),
                Error::<T>::InvariantViolation
            );
        }
        Ok(total)
    }
}
