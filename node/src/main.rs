//! Substrate Node Template CLI library.
#![warn(missing_docs)]
#![feature(type_alias_impl_trait)] // 启用 const generics 功能

use sp_core::bounded::alloc::vec::Vec;

mod benchmarking;
mod chain_spec;
mod cli;
mod command;
mod rpc;
mod service;
mod sunlead_consensus;
mod node_primitives;
mod eth;
mod client;
mod exchange_pool;
mod exchange_p2p;
mod exchange_proposer;
mod pricefeed;
mod oracle_feed;
// Mirrors `lib.rs`: these files are compiled into both targets, so a module declared in one
// list and missing from the other simply does not resolve here.
mod oracle_keys;
mod premium_feed;

fn main() -> sc_cli::Result<()> {
	command::run(Option::<Vec<String>>::None)
}
