use alloc::{collections::BTreeMap, vec, vec::Vec};
use codec::{Decode, Encode};
use exchange_primitives::{Lots, MarketId};
use scale_info::TypeInfo;
use sp_core::hashing::blake2_256;

use crate::{
    orderbook::OrderBook,
    types::{
        AccountId, ApplyOutcome, Command, CommandKind, EngineEvent, EngineSnapshot, Execution,
        OrderId, OrderRequest, RestingOrder, Side, TimeInForce, Trade,
    },
};

/// Fatal protocol/state errors. They are distinct from deterministic business rejections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineError {
    SequenceMismatch { expected: u64, got: u64 },
    NonceNotIncreasing { previous: u64, got: u64 },
    OrderIdExhausted,
    CorruptBook,
    SnapshotVersionMismatch,
    SnapshotDecodeFailed,
}

/// Deterministic v1 in-memory matching state machine.
///
/// BTreeMap provides canonical iteration and SCALE encoding. No wall clock, randomness,
/// thread scheduling, network result or local arrival timestamp influences execution.
/// Signature verification, risk checks, collateral reservation and settlement are adapters
/// owned by the Gateway/Runtime and are deliberately outside this first extraction.
#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct MatchingEngine {
    engine_version: u32,
    last_sequence: u64,
    next_order_id: OrderId,
    books: BTreeMap<MarketId, OrderBook>,
    orders: BTreeMap<OrderId, RestingOrder>,
    client_index: BTreeMap<(AccountId, u32, u64), OrderId>,
    account_nonces: BTreeMap<(AccountId, u32), u64>,
}

impl Default for MatchingEngine {
    fn default() -> Self { Self::new() }
}

impl MatchingEngine {
    pub const ENGINE_VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            engine_version: Self::ENGINE_VERSION,
            last_sequence: 0,
            next_order_id: 1,
            books: BTreeMap::new(),
            orders: BTreeMap::new(),
            client_index: BTreeMap::new(),
            account_nonces: BTreeMap::new(),
        }
    }

    pub fn last_sequence(&self) -> u64 { self.last_sequence }

    pub fn order(&self, id: OrderId) -> Option<&RestingOrder> { self.orders.get(&id) }

    pub fn order_by_client_id(
        &self,
        owner: &AccountId,
        subaccount: u32,
        client_order_id: u64,
    ) -> Option<&RestingOrder> {
        let id = self.client_index.get(&(*owner, subaccount, client_order_id))?;
        self.orders.get(id)
    }

    pub fn book(&self, market_id: MarketId) -> Option<&OrderBook> { self.books.get(&market_id) }

    /// Commitment to the complete engine state, not merely the visible order book.
    pub fn state_root(&self) -> [u8; 32] { blake2_256(&self.encode()) }

    /// Apply one sequenced command.
    ///
    /// A business rejection consumes the sequence and nonce, matching the pallet's distinction
    /// between a pre-execution refusal and a rejected action that reached execution. Sequence
    /// and nonce violations are fatal protocol errors and must cause the enclosing block/state
    /// transaction to abort. The caller must not persist state after receiving Err.
    pub fn apply(&mut self, command: &Command) -> Result<Execution, EngineError> {
        let expected = self.last_sequence.checked_add(1).ok_or(EngineError::SequenceMismatch {
            expected: u64::MAX,
            got: command.sequence,
        })?;
        if command.sequence != expected {
            return Err(EngineError::SequenceMismatch { expected, got: command.sequence });
        }
        let nonce_key = (command.owner, command.subaccount);
        let previous = self.account_nonces.get(&nonce_key).copied().unwrap_or(0);
        if command.nonce <= previous {
            return Err(EngineError::NonceNotIncreasing { previous, got: command.nonce });
        }

        // Commit protocol counters before business logic: deterministic business rejections
        // consume nonce. Fatal errors require the caller's enclosing transaction to roll back.
        self.last_sequence = command.sequence;
        self.account_nonces.insert(nonce_key, command.nonce);

        let (outcome, events, remaining_lots) = match &command.kind {
            CommandKind::Place(request) => self.apply_place(command, request)?,
            CommandKind::Cancel(request) => self.apply_cancel(command, request)?,
        };

        Ok(Execution {
            sequence: command.sequence,
            owner: command.owner,
            subaccount: command.subaccount,
            nonce: command.nonce,
            outcome,
            events,
            remaining_lots,
            state_root: self.state_root(),
        })
    }

    fn apply_place(
        &mut self,
        command: &Command,
        request: &OrderRequest,
    ) -> Result<(ApplyOutcome, Vec<EngineEvent>, Lots), EngineError> {
        if request.quantity_lots == 0 || request.price_ticks == 0 {
            return Ok((ApplyOutcome::RejectedInvalidOrder, vec![], 0));
        }
        let client_key = (command.owner, command.subaccount, request.client_order_id);
        if self.client_index.contains_key(&client_key) {
            return Ok((ApplyOutcome::RejectedDuplicateClientOrderId, vec![], 0));
        }

        let crosses = self.books.get(&request.market_id)
            .and_then(|book| book.best_price(request.side.opposite()))
            .map(|best| match request.side {
                Side::Buy => request.price_ticks >= best,
                Side::Sell => request.price_ticks <= best,
            })
            .unwrap_or(false);
        if request.post_only && crosses {
            return Ok((ApplyOutcome::RejectedPostOnlyWouldTake, vec![], 0));
        }

        let order_id = self.next_order_id;
        self.next_order_id = self.next_order_id.checked_add(1).ok_or(EngineError::OrderIdExhausted)?;
        let mut remaining = request.quantity_lots;
        let mut total_filled = 0u64;
        let mut events = vec![EngineEvent::OrderAccepted { order_id }];

        while remaining > 0 {
            let maker_id = match self.books.get(&request.market_id)
                .and_then(|book| book.best_order_id(request.side.opposite())) {
                Some(id) => id,
                None => break,
            };
            let maker = self.orders.get(&maker_id).cloned().ok_or(EngineError::CorruptBook)?;
            let crosses_price = match request.side {
                Side::Buy => request.price_ticks >= maker.price_ticks,
                Side::Sell => request.price_ticks <= maker.price_ticks,
            };
            if !crosses_price { break; }

            let fill = core::cmp::min(remaining, maker.remaining_lots);
            remaining = remaining.checked_sub(fill).ok_or(EngineError::CorruptBook)?;
            total_filled = total_filled.checked_add(fill).ok_or(EngineError::CorruptBook)?;
            let maker_remaining = maker.remaining_lots.checked_sub(fill).ok_or(EngineError::CorruptBook)?;
            let trade = Trade {
                maker_order_id: maker.id,
                taker_order_id: order_id,
                maker: maker.owner,
                taker: command.owner,
                market_id: request.market_id,
                price_ticks: maker.price_ticks,
                lots: fill,
            };
            events.push(EngineEvent::TradeExecuted(trade));

            if maker_remaining == 0 {
                let popped = self.books.get_mut(&request.market_id)
                    .and_then(|book| book.pop_best(maker.side, fill))
                    .ok_or(EngineError::CorruptBook)?;
                if popped != maker_id { return Err(EngineError::CorruptBook); }
                self.orders.remove(&maker_id);
                self.client_index.remove(&(maker.owner, maker.subaccount, maker.client_order_id));
                events.push(EngineEvent::OrderFilled { order_id: maker_id });
            } else {
                let stored = self.orders.get_mut(&maker_id).ok_or(EngineError::CorruptBook)?;
                stored.remaining_lots = maker_remaining;
                let level = self.books.get_mut(&request.market_id)
                    .and_then(|book| book.level_mut(maker.side, maker.price_ticks))
                    .ok_or(EngineError::CorruptBook)?;
                level.total_lots = level.total_lots.checked_sub(fill).ok_or(EngineError::CorruptBook)?;
                events.push(EngineEvent::OrderPartiallyFilled { order_id: maker_id, filled_lots: fill });
            }
        }

        if total_filled > 0 {
            if remaining == 0 {
                events.push(EngineEvent::OrderFilled { order_id });
            } else {
                events.push(EngineEvent::OrderPartiallyFilled { order_id, filled_lots: total_filled });
            }
        }

        if remaining > 0 && request.tif == TimeInForce::GoodTilCancelled {
            let resting = RestingOrder {
                id: order_id,
                owner: command.owner,
                subaccount: command.subaccount,
                market_id: request.market_id,
                client_order_id: request.client_order_id,
                side: request.side,
                price_ticks: request.price_ticks,
                original_lots: request.quantity_lots,
                remaining_lots: remaining,
                sequence: command.sequence,
            };
            let inserted = self.books.entry(request.market_id).or_default()
                .insert(request.side, request.price_ticks, order_id, remaining);
            if !inserted { return Err(EngineError::CorruptBook); }
            self.orders.insert(order_id, resting);
            self.client_index.insert(client_key, order_id);
        }

        Ok((ApplyOutcome::Applied, events, remaining))
    }

    fn apply_cancel(
        &mut self,
        command: &Command,
        request: &crate::types::CancelOrder,
    ) -> Result<(ApplyOutcome, Vec<EngineEvent>, Lots), EngineError> {
        let key = (command.owner, command.subaccount, request.client_order_id);
        let Some(order_id) = self.client_index.get(&key).copied() else {
            return Ok((ApplyOutcome::RejectedOrderNotFound, vec![], 0));
        };
        let order = self.orders.get(&order_id).cloned().ok_or(EngineError::CorruptBook)?;
        if order.market_id != request.market_id {
            return Ok((ApplyOutcome::RejectedOrderNotFound, vec![], 0));
        }
        let removed = self.books.get_mut(&order.market_id)
            .map(|book| book.remove(order.side, order.price_ticks, order.id, order.remaining_lots))
            .unwrap_or(false);
        if !removed { return Err(EngineError::CorruptBook); }
        self.orders.remove(&order_id);
        self.client_index.remove(&key);
        Ok((ApplyOutcome::Applied, vec![EngineEvent::OrderCancelled { order_id }], 0))
    }

    /// Verify cross-index and aggregate invariants. Also run on snapshot restore.
    pub fn validate_invariants(&self) -> Result<(), EngineError> {
        let mut seen = BTreeMap::<OrderId, ()>::new();
        for (market_id, book) in &self.books {
            for (side, levels) in [(Side::Buy, &book.bids), (Side::Sell, &book.asks)] {
                for (price, level) in levels {
                    if level.orders.is_empty() { return Err(EngineError::CorruptBook); }
                    let mut total = 0u64;
                    for id in &level.orders {
                        if seen.insert(*id, ()).is_some() { return Err(EngineError::CorruptBook); }
                        let order = self.orders.get(id).ok_or(EngineError::CorruptBook)?;
                        if order.market_id != *market_id || order.side != side || order.price_ticks != *price || order.remaining_lots == 0 {
                            return Err(EngineError::CorruptBook);
                        }
                        total = total.checked_add(order.remaining_lots).ok_or(EngineError::CorruptBook)?;
                        let key = (order.owner, order.subaccount, order.client_order_id);
                        if self.client_index.get(&key) != Some(id) { return Err(EngineError::CorruptBook); }
                    }
                    if total != level.total_lots { return Err(EngineError::CorruptBook); }
                }
            }
        }
        if seen.len() != self.orders.len() || self.client_index.len() != self.orders.len() {
            return Err(EngineError::CorruptBook);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot { engine_version: self.engine_version, state: self.encode() }
    }

    pub fn restore(snapshot: EngineSnapshot) -> Result<Self, EngineError> {
        if snapshot.engine_version != Self::ENGINE_VERSION { return Err(EngineError::SnapshotVersionMismatch); }
        let engine = Self::decode(&mut &snapshot.state[..]).map_err(|_| EngineError::SnapshotDecodeFailed)?;
        if engine.engine_version != Self::ENGINE_VERSION { return Err(EngineError::SnapshotVersionMismatch); }
        engine.validate_invariants()?;
        Ok(engine)
    }

    pub fn depth(&self, market_id: MarketId, side: Side, limit: usize) -> Vec<(u64, Lots, usize)> {
        self.books.get(&market_id).map(|book| book.depth(side, limit)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CancelOrder, CommandKind};

    fn command(sequence: u64, owner: u8, nonce: u64, kind: CommandKind) -> Command {
        Command { sequence, owner: [owner; 32], subaccount: 0, nonce, kind }
    }
    fn place(side: Side, price: u64, lots: u64, client_order_id: u64) -> CommandKind {
        CommandKind::Place(OrderRequest {
            market_id: 1, client_order_id, side, price_ticks: price, quantity_lots: lots,
            tif: TimeInForce::GoodTilCancelled, post_only: false,
        })
    }

    #[test]
    fn price_priority_and_maker_price() {
        let mut engine = MatchingEngine::new();
        engine.apply(&command(1, 1, 1, place(Side::Sell, 101, 5, 1))).unwrap();
        engine.apply(&command(2, 2, 1, place(Side::Sell, 100, 5, 2))).unwrap();
        let execution = engine.apply(&command(3, 3, 1, place(Side::Buy, 105, 7, 3))).unwrap();
        let trades: Vec<_> = execution.events.iter().filter_map(|event| match event {
            EngineEvent::TradeExecuted(trade) => Some(trade), _ => None,
        }).collect();
        assert_eq!(trades.len(), 2);
        assert_eq!((trades[0].price_ticks, trades[0].lots), (100, 5));
        assert_eq!((trades[1].price_ticks, trades[1].lots), (101, 2));
        assert_eq!(execution.remaining_lots, 0);
        engine.validate_invariants().unwrap();
    }

    #[test]
    fn fifo_within_price_level() {
        let mut engine = MatchingEngine::new();
        engine.apply(&command(1, 1, 1, place(Side::Sell, 100, 2, 1))).unwrap();
        engine.apply(&command(2, 2, 1, place(Side::Sell, 100, 3, 2))).unwrap();
        let execution = engine.apply(&command(3, 3, 1, place(Side::Buy, 100, 3, 3))).unwrap();
        let makers: Vec<_> = execution.events.iter().filter_map(|event| match event {
            EngineEvent::TradeExecuted(trade) => Some(trade.maker_order_id), _ => None,
        }).collect();
        assert_eq!(makers, vec![1, 2]);
        assert_eq!(engine.order(2).unwrap().remaining_lots, 2);
        engine.validate_invariants().unwrap();
    }

    #[test]
    fn post_only_crossing_order_is_rejected_but_nonce_is_consumed() {
        let mut engine = MatchingEngine::new();
        engine.apply(&command(1, 1, 1, place(Side::Sell, 100, 1, 1))).unwrap();
        let mut request = match place(Side::Buy, 100, 1, 2) { CommandKind::Place(r) => r, _ => unreachable!() };
        request.post_only = true;
        let result = engine.apply(&command(2, 2, 1, CommandKind::Place(request))).unwrap();
        assert_eq!(result.outcome, ApplyOutcome::RejectedPostOnlyWouldTake);
        assert_eq!(engine.last_sequence(), 2);
    }

    #[test]
    fn cancel_removes_order_and_empty_level() {
        let mut engine = MatchingEngine::new();
        engine.apply(&command(1, 1, 1, place(Side::Buy, 99, 4, 77))).unwrap();
        let result = engine.apply(&command(2, 1, 2, CommandKind::Cancel(CancelOrder { market_id: 1, client_order_id: 77 }))).unwrap();
        assert_eq!(result.outcome, ApplyOutcome::Applied);
        assert!(engine.order_by_client_id(&[1; 32], 0, 77).is_none());
        assert!(engine.depth(1, Side::Buy, 10).is_empty());
        engine.validate_invariants().unwrap();
    }

    #[test]
    fn snapshot_round_trip_preserves_state_root() {
        let mut engine = MatchingEngine::new();
        engine.apply(&command(1, 1, 1, place(Side::Buy, 99, 4, 1))).unwrap();
        let restored = MatchingEngine::restore(engine.snapshot()).unwrap();
        assert_eq!(engine.state_root(), restored.state_root());
    }

    #[test]
    fn sequence_must_be_contiguous() {
        let mut engine = MatchingEngine::new();
        let error = engine.apply(&command(2, 1, 1, place(Side::Buy, 99, 1, 1))).unwrap_err();
        assert_eq!(error, EngineError::SequenceMismatch { expected: 1, got: 2 });
        assert_eq!(engine.last_sequence(), 0);
    }
}
