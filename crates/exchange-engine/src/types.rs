use alloc::vec::Vec;
use codec::{Decode, Encode};
use exchange_primitives::{Lots, MarketId, PriceTicks, SubaccountId};
use scale_info::TypeInfo;

pub type AccountId = [u8; 32];
pub type OrderId = u64;
pub type ClientOrderId = u64;

#[derive(Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub enum Side { Buy, Sell }
impl Side {
    pub const fn opposite(self) -> Self {
        match self { Self::Buy => Self::Sell, Self::Sell => Self::Buy }
    }
}

#[derive(Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub enum TimeInForce { GoodTilCancelled, ImmediateOrCancel }

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct OrderRequest {
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub quantity_lots: Lots,
    pub tif: TimeInForce,
    pub post_only: bool,
}

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct CancelOrder { pub market_id: MarketId, pub client_order_id: ClientOrderId }

/// Sequence is assigned by the protocol-facing sequencer; signature verification is external.
#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct Command {
    pub sequence: u64,
    pub owner: AccountId,
    pub subaccount: SubaccountId,
    pub nonce: u64,
    pub kind: CommandKind,
}

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub enum CommandKind { Place(OrderRequest), Cancel(CancelOrder) }

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct RestingOrder {
    pub id: OrderId,
    pub owner: AccountId,
    pub subaccount: SubaccountId,
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub original_lots: Lots,
    pub remaining_lots: Lots,
    pub sequence: u64,
}

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct Trade {
    pub maker_order_id: OrderId,
    pub taker_order_id: OrderId,
    pub maker: AccountId,
    pub taker: AccountId,
    pub market_id: MarketId,
    pub price_ticks: PriceTicks,
    pub lots: Lots,
}

#[derive(Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub enum ApplyOutcome {
    Applied,
    RejectedInvalidOrder,
    RejectedPostOnlyWouldTake,
    RejectedOrderNotFound,
    RejectedDuplicateClientOrderId,
}

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub enum EngineEvent {
    OrderAccepted { order_id: OrderId },
    OrderPartiallyFilled { order_id: OrderId, filled_lots: Lots },
    OrderFilled { order_id: OrderId },
    OrderCancelled { order_id: OrderId },
    TradeExecuted(Trade),
}

/// Business rejections consume sequence and nonce. Protocol sequence/nonce errors are fatal.
#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct Execution {
    pub sequence: u64,
    pub owner: AccountId,
    pub subaccount: SubaccountId,
    pub nonce: u64,
    pub outcome: ApplyOutcome,
    pub events: Vec<EngineEvent>,
    /// Unfilled quantity of a newly submitted order (including IOC remainder).
    pub remaining_lots: Lots,
    pub state_root: [u8; 32],
}

#[derive(Clone, Debug, Encode, Decode, Eq, PartialEq, TypeInfo)]
pub struct EngineSnapshot { pub engine_version: u32, pub state: Vec<u8> }
