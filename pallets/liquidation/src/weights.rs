//! Weights for `pallet-liquidation`.
//!
//! This pallet does almost no work of its own — it forwards to `ExchangeEngine`, pays the
//! keeper and emits events — so its weights are expressed as a function of
//! `pallet-exchange-core`'s measured components rather than as independent constants. That
//! keeps one source of truth: re-running the engine's benchmarks updates these too.
//!
//! **Caveat**: the engine's own `place_order` / `adl_close` numbers still need to be measured
//! on target hardware (see the module docs in `pallet-exchange-core/src/weights.rs`), and this
//! pallet has no benchmarks of its own yet.

use core::marker::PhantomData;
use frame_support::{
	traits::Get,
	weights::{constants::RocksDbWeight, Weight},
};

pub trait WeightInfo {
	/// Permissionless liquidation of a single account.
	fn liquidate() -> Weight;
	/// ADL over `targets` candidate accounts.
	fn adl(targets: u32) -> Weight;
}

/// A trivial implementation for tests.
///
/// `SubstrateWeight` below is generic over `pallet_exchange_core::Config`, which the exchange
/// core's own mock cannot satisfy: the dev-dependency cycle (core --dev--> liquidation --> core)
/// makes cargo build *two* instances of `pallet-exchange-core`, and the mock's `Test` only
/// implements `Config` for one of them. Tests do not care about accurate weights, so they use
/// this instead.
impl WeightInfo for () {
	fn liquidate() -> Weight {
		Weight::from_parts(350_000_000, 12_000)
			.saturating_add(RocksDbWeight::get().reads_writes(12, 12))
	}
	fn adl(targets: u32) -> Weight {
		Weight::from_parts(150_000_000, 8_000)
			.saturating_add(RocksDbWeight::get().reads_writes(3, 3))
			.saturating_mul(targets.max(1) as u64)
			.saturating_add(RocksDbWeight::get().reads_writes(2, 1))
	}
}

/// Weights derived from the exchange core's measured components.
pub struct SubstrateWeight<T>(PhantomData<T>);

impl<T: pallet_exchange_core::Config> WeightInfo for SubstrateWeight<T> {
	fn liquidate() -> Weight {
		use pallet_exchange_core::weights::WeightInfo as _;
		// Worst case is a reduce-only order that sweeps the whole match budget, plus the
		// settlement, ranking, penalty and insurance-credit work wrapped around it.
		pallet_exchange_core::weights::SubstrateWeight::<T>::place_order(
			T::MaxMatchesPerAction::get(),
			T::MaxMatchesPerAction::get(),
			T::MaxPriceLevelsPerSide::get(),
		)
		.saturating_add(T::DbWeight::get().reads_writes(2, 2))
	}

	fn adl(targets: u32) -> Weight {
		use pallet_exchange_core::weights::WeightInfo as _;
		// Every target costs one engine-side offset plus the read used to rank it.
		let per_target = pallet_exchange_core::weights::SubstrateWeight::<T>::adl_close()
			.saturating_add(T::DbWeight::get().reads_writes(1, 1));
		per_target
			.saturating_mul(targets.max(1) as u64)
			.saturating_add(T::DbWeight::get().reads_writes(2, 1))
	}
}
