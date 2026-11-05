use alloc::{collections::{BTreeMap, VecDeque}, vec::Vec};
use codec::{Decode, Encode};
use exchange_primitives::{Lots, PriceTicks};
use scale_info::TypeInfo;
use crate::types::{OrderId, Side};

#[derive(Clone, Debug, Default, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct PriceLevel {
    pub orders: VecDeque<OrderId>,
    pub total_lots: Lots,
}

#[derive(Clone, Debug, Default, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct OrderBook {
    /// Ascending keys; best bid is the greatest key.
    pub bids: BTreeMap<PriceTicks, PriceLevel>,
    /// Ascending keys; best ask is the smallest key.
    pub asks: BTreeMap<PriceTicks, PriceLevel>,
}

impl OrderBook {
    pub fn best_price(&self, side: Side) -> Option<PriceTicks> {
        match side { Side::Buy => self.bids.keys().next_back().copied(), Side::Sell => self.asks.keys().next().copied() }
    }
    pub fn best_order_id(&self, side: Side) -> Option<OrderId> {
        let price = self.best_price(side)?;
        self.level(side, price)?.orders.front().copied()
    }
    pub fn level(&self, side: Side, price: PriceTicks) -> Option<&PriceLevel> {
        match side { Side::Buy => self.bids.get(&price), Side::Sell => self.asks.get(&price) }
    }
    pub(crate) fn level_mut(&mut self, side: Side, price: PriceTicks) -> Option<&mut PriceLevel> {
        match side { Side::Buy => self.bids.get_mut(&price), Side::Sell => self.asks.get_mut(&price) }
    }
    pub(crate) fn insert(&mut self, side: Side, price: PriceTicks, id: OrderId, lots: Lots) -> bool {
        let levels = match side { Side::Buy => &mut self.bids, Side::Sell => &mut self.asks };
        let level = levels.entry(price).or_default();
        let Some(total) = level.total_lots.checked_add(lots) else { return false };
        level.orders.push_back(id);
        level.total_lots = total;
        true
    }
    pub(crate) fn pop_best(&mut self, side: Side, filled_lots: Lots) -> Option<OrderId> {
        let price = self.best_price(side)?;
        let levels = match side { Side::Buy => &mut self.bids, Side::Sell => &mut self.asks };
        let level = levels.get_mut(&price)?;
        let total = level.total_lots.checked_sub(filled_lots)?;
        let id = level.orders.pop_front()?;
        level.total_lots = total;
        if level.orders.is_empty() {
            if level.total_lots != 0 { return None; }
            levels.remove(&price);
        }
        Some(id)
    }
    pub(crate) fn remove(&mut self, side: Side, price: PriceTicks, id: OrderId, lots: Lots) -> bool {
        let levels = match side { Side::Buy => &mut self.bids, Side::Sell => &mut self.asks };
        let Some(level) = levels.get_mut(&price) else { return false };
        let Some(index) = level.orders.iter().position(|candidate| *candidate == id) else { return false };
        let Some(total) = level.total_lots.checked_sub(lots) else { return false };
        level.orders.remove(index);
        level.total_lots = total;
        if level.orders.is_empty() {
            if level.total_lots != 0 { return false; }
            levels.remove(&price);
        }
        true
    }
    pub fn depth(&self, side: Side, limit: usize) -> Vec<(PriceTicks, Lots, usize)> {
        let mut result = Vec::new();
        match side {
            Side::Buy => for (price, level) in self.bids.iter().rev().take(limit) { result.push((*price, level.total_lots, level.orders.len())); },
            Side::Sell => for (price, level) in self.asks.iter().take(limit) { result.push((*price, level.total_lots, level.orders.len())); },
        }
        result
    }
}
