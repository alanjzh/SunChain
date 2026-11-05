//! Deterministic in-memory order book and matching state machine.
//!
//! v1 implements price-time priority, FIFO price levels, GTC/IOC, post-only rejection,
//! cancellation, command sequencing, snapshots and a SCALE/Blake2 state commitment.
//! It deliberately excludes margin, funding, liquidation, custody and settlement.
//! Runtime remains authoritative: this crate is not yet wired into block production/import.
//!
//! The first implementation favors auditable semantics over micro-optimization. Removing
//! an order from the middle of a level is O(level depth); an intrusive arena can replace this
//! after differential tests establish exact equivalence with pallet-exchange-core.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod engine;
mod orderbook;
mod types;

pub use engine::{EngineError, MatchingEngine};
pub use orderbook::{OrderBook, PriceLevel};
pub use types::{
    AccountId, ApplyOutcome, CancelOrder, Command, CommandKind, EngineEvent, EngineSnapshot,
    Execution, OrderId, OrderRequest, RestingOrder, Side, TimeInForce, Trade,
};
