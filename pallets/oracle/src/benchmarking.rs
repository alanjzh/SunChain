//! Benchmarks for `pallet-oracle`.
//!
//! The pallet has no `weights.rs` yet, so the runtime is running on the trivial weights the
//! macro generates. These produce the real ones.
//!
//! The interesting cases are the two ways in a price arrives: the signed `submit_prices` and
//! the injected `submit_price_vote`, whose cost includes verifying an inner signature.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use crate::{Config, Pallet, Quotes};
use frame_benchmarking::v2::*;
use frame_support::{assert_ok, traits::Get};
use frame_system::pallet_prelude::BlockNumberFor;
use frame_system::RawOrigin;
use sp_std::vec::Vec;

/// A whitelisted source to submit as.
fn source<T: Config>(index: u32) -> T::AccountId {
	frame_benchmarking::account("oracle-source", index, 0)
}

/// Clears the pallet's state.
///
/// A benchmark with more than one component runs its setup once per component, so setup must
/// not accumulate: repeated `set_source` blows through `MaxSources` and repeated
/// `track_market` returns `MarketAlreadyTracked`. Both surface only as a WASM `unreachable`.
fn reset() {
	// Byte-string literals have different lengths, so coerce them to slices.
	for item in [
		&b"Sources"[..],
		&b"TrackedMarkets"[..],
		&b"Quotes"[..],
		&b"MarketPrices"[..],
	] {
		frame_support::storage::unhashed::kill(&frame_support::storage::storage_prefix(
			b"Oracle", item,
		));
	}
}

/// Registers `count` sources and tracks one market. Idempotent.
fn setup<T: Config>(sources: u32) -> T::AccountId {
	reset();
	for i in 0..sources {
		let who = source::<T>(i);
		assert_ok!(Pallet::<T>::set_source(RawOrigin::Root.into(), who, true));
	}
	assert_ok!(Pallet::<T>::track_market(RawOrigin::Root.into(), 1, true));
	source::<T>(0)
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn submit_prices(p: Linear<1, 64>, s: Linear<2, 8>) {
		// `s` is how many sources already have a quote for the market, i.e. how much state the
		// writes land in.
		let who = setup::<T>(s);
		let prices: Vec<_> = (0..p).map(|i| (1u32 + i, 60_000u64)).collect();

		#[extrinsic_call]
		submit_prices(RawOrigin::Signed(who), prices.try_into().unwrap());
	}

	// `submit_price_vote` is deliberately **not** benchmarked: a valid inner signature cannot
	// be produced generically, and benchmarking it with an invalid one would measure the
	// *failing* path (it returns `BadVoteSignature` early) and therefore underestimate the
	// weight — silently, which is the worst way to be wrong. Its declared weight reuses
	// `submit_prices` plus the cost of one signature recovery; see `weights.rs` when it is
	// generated.

	#[benchmark]
	fn set_source() {
		let who = source::<T>(1);

		#[extrinsic_call]
		set_source(RawOrigin::Root, who, true);
	}

	#[benchmark]
	fn track_market() {
		reset();
		#[extrinsic_call]
		track_market(RawOrigin::Root, 2u32, true);
	}

	#[benchmark]
	fn force_set_price() {
		reset();
		assert_ok!(Pallet::<T>::track_market(RawOrigin::Root.into(), 3, true));

		#[extrinsic_call]
		force_set_price(RawOrigin::Root, 3u32, 60_000u64);
	}

	#[benchmark]
	fn on_initialize(m: Linear<1, 64>, s: Linear<1, 8>) {
		reset();
		// Fill a quote per source per market, so the hook has real work to aggregate.
		for market in 0..m {
			assert_ok!(Pallet::<T>::track_market(
				RawOrigin::Root.into(),
				market,
				true
			));
			for i in 0..s {
				let who = source::<T>(i);
				Quotes::<T>::insert(market, who, Quote { price_ticks: 60_000, at: Default::default(), nonce: 1 });
			}
		}

		#[block]
		{
			<Pallet<T> as frame_support::traits::Hooks<
				BlockNumberFor<T>,
			>>::on_initialize(0u32.into());
		}
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
