//! Migration test running against the **real runtime**.
//!
//! `try-runtime-cli` is not vendored in this SDK checkout, so `try-runtime on-runtime-upgrade`
//! cannot be run. This test stands in for it: it loads a state holding *Phase 1 encoded*
//! `Position` values and then drives the migration's `pre_upgrade` / `on_runtime_upgrade` /
//! `post_upgrade` hooks.
//!
//! What this adds over the pallet's own unit tests is precisely what a mock cannot check: the
//! real `construct_runtime!` pallet prefix, the real hasher and the real `Position` type.
//!
//! ## Why the state is built as a `Storage`, not with `put_raw`
//!
//! `sp_io::storage::next_key` — which is what `StorageDoubleMap::iter()` and `translate()` are
//! built on — walks the **backend**, not the overlay. Entries written with `unhashed::put_raw`
//! (even after `commit_all()`) are addressable by key via `get_raw`, but are *not* returned by
//! prefix iteration. That makes `pre_upgrade` count 0 while `post_upgrade` counts 2, which
//! looks exactly like data loss during the migration when nothing was lost at all.
//!
//! ## `frame-remote-externalities`
//!
//! Tried first, since it is the path the CLI itself uses, but its
//! `TestExternalities::from_raw_snapshot` *validates* the state root, so a synthetic snapshot
//! needs a bit-exact trie root. Reproducing that proved more brittle than it is worth; if a
//! real chain dump ever becomes available, load it with `Mode::Offline` and reuse the rest.

use sunchain_runtime::Runtime;
use exchange_primitives::DEFAULT_SUBACCOUNT;
use codec::Encode;
use frame_support::{
	storage::StorageDoubleMap,
	traits::UncheckedOnRuntimeUpgrade,
};
use sp_core::storage::Storage;
use sp_io::TestExternalities;

/// `Position` exactly as Phase 1 encoded it: 32 bytes, no trailing `funding_index`.
fn old_position(size_lots: i64, entry_price_ticks: u64, realized_pnl: i128) -> Vec<u8> {
	let mut v = Vec::new();
	v.extend_from_slice(&size_lots.encode());
	v.extend_from_slice(&entry_price_ticks.encode());
	v.extend_from_slice(&realized_pnl.encode());
	assert_eq!(v.len(), 32, "the Phase 1 encoding must be 32 bytes");
	v
}

#[test]
fn migration_v1_upgrades_old_positions_under_the_real_runtime() {
	const MARKET: u32 = 0;
	let alice = sunchain_runtime::AccountId::from([1u8; 32]);
	let bob = sunchain_runtime::AccountId::from([2u8; 32]);

	// Real runtime storage keys — computed by the very code the runtime uses.
	let alice_key =
		pallet_exchange_core::Positions::<Runtime>::hashed_key_for((alice.clone(), DEFAULT_SUBACCOUNT, MARKET));
	let bob_key = pallet_exchange_core::Positions::<Runtime>::hashed_key_for((bob.clone(), DEFAULT_SUBACCOUNT, MARKET));

	let storage = Storage {
		top: vec![
			(alice_key, old_position(1_000, 60_000, 0)),
			(bob_key, old_position(-250, 57_500, -1_250)),
		]
		.into_iter()
		.collect(),
		children_default: Default::default(),
	};

	TestExternalities::new(storage).execute_with(|| {
		type Migration = pallet_exchange_core::migrations::v1::Migration<Runtime>;

		// The pre/post hooks only exist under `try-runtime`; run this test with
		// `--features try-runtime` to exercise them.
		#[cfg(feature = "try-runtime")]
		{
			let state_before = Migration::pre_upgrade().expect("pre_upgrade");
			Migration::on_runtime_upgrade();
			Migration::post_upgrade(state_before).expect("post_upgrade");
		}
		#[cfg(not(feature = "try-runtime"))]
		Migration::on_runtime_upgrade();

		let alice_pos =
			pallet_exchange_core::Positions::<Runtime>::get((alice, DEFAULT_SUBACCOUNT, MARKET)).expect("present");
		assert_eq!(alice_pos.size_lots, 1_000);
		assert_eq!(alice_pos.entry_price_ticks, 60_000);
		assert_eq!(alice_pos.realized_pnl, 0);
		assert_eq!(alice_pos.funding_index, 0, "no market entry -> seeded from 0");

		let bob_pos =
			pallet_exchange_core::Positions::<Runtime>::get((bob, DEFAULT_SUBACCOUNT, MARKET)).expect("present");
		assert_eq!(bob_pos.size_lots, -250);
		assert_eq!(bob_pos.entry_price_ticks, 57_500);
		assert_eq!(bob_pos.realized_pnl, -1_250);
	});
}

/// v3 -> v4: `Order` gained a `subaccount` field (S3-1), which shifts the SCALE layout by 4
/// bytes. A pre-S3-1 order no longer decodes as the current `Order`; `Orders::iter()` would
/// silently skip it, leaving garbage in storage forever. This test drives the real v4 migration
/// against the real runtime: a current order survives, a legacy one is dropped.
#[test]
fn migration_v4_drops_pre_s3_1_orders_that_no_longer_decode() {
	const MARKET: u32 = 0;
	let alice = sunchain_runtime::AccountId::from([1u8; 32]);

	let current = pallet_exchange_core::Order::<
		sunchain_runtime::AccountId,
		sunchain_runtime::BlockNumber,
	> {
		owner: alice.clone(),
		subaccount: DEFAULT_SUBACCOUNT,
		market_id: MARKET,
		client_order_id: 1,
		side: pallet_exchange_core::Side::Buy,
		price_ticks: 60_000,
		original_lots: 100,
		remaining_lots: 100,
		reserved_margin: 1_000,
		tif: pallet_exchange_core::TimeInForce::GoodTilCancelled,
		post_only: false,
		reduce_only: false,
		placed_at: 1,
		expires_at: 0,
		prev_order: None,
		next_order: None,
	};
	let current_bytes = current.encode();

	// The pre-S3-1 layout is the same order *without* `subaccount`. With a 32-byte owner the
	// field is the 4 bytes immediately after it, so slicing them out reconstructs a legacy
	// encoding exactly — one that fails to decode as the current `Order`, which is precisely
	// what makes the migration drop it.
	assert_eq!(&current_bytes[32..36], &DEFAULT_SUBACCOUNT.encode()[..]);
	let mut legacy_bytes = current_bytes[..32].to_vec();
	legacy_bytes.extend_from_slice(&current_bytes[36..]);

	let keep_key = pallet_exchange_core::Orders::<Runtime>::hashed_key_for(MARKET, 1u64);
	let drop_key = pallet_exchange_core::Orders::<Runtime>::hashed_key_for(MARKET, 2u64);

	let storage = Storage {
		top: vec![(keep_key, current_bytes), (drop_key, legacy_bytes)]
			.into_iter()
			.collect(),
		children_default: Default::default(),
	};

	TestExternalities::new(storage).execute_with(|| {
		type Migration = pallet_exchange_core::migrations::v4::Migration<Runtime>;

		#[cfg(feature = "try-runtime")]
		{
			let state_before = Migration::pre_upgrade().expect("pre_upgrade");
			Migration::on_runtime_upgrade();
			Migration::post_upgrade(state_before).expect("post_upgrade");
		}
		#[cfg(not(feature = "try-runtime"))]
		Migration::on_runtime_upgrade();

		assert!(
			pallet_exchange_core::Orders::<Runtime>::get(MARKET, 1u64).is_some(),
			"a current order survives the migration"
		);
		assert!(
			pallet_exchange_core::Orders::<Runtime>::get(MARKET, 2u64).is_none(),
			"a legacy order that no longer decodes is dropped"
		);
	});
}
