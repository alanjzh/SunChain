//! Storage migrations.
//!
//! Phase 1 never declared a storage version, so any chain that already holds Phase 1 state is
//! at version `0`. The P2-B layout — `Position::funding_index` plus the new `collateral`
//! semantics — is version `1`, and this module is the only supported way between the two
//! (Phase 2 design §10).

use super::*;
use alloc::vec::Vec;
use codec::Decode;
use exchange_primitives::DEFAULT_SUBACCOUNT;
use frame_support::storage::unhashed;
use frame_support::{traits::Get, weights::Weight};

// `Vec` comes from `alloc::vec::Vec` above; `sp_std::vec` is the same item, and importing
// it a second time under `try-runtime` is E0252 — which is why `--all-features` used to
// fail while the default build stayed green.
#[cfg(feature = "try-runtime")]
use codec::Encode;
#[cfg(feature = "try-runtime")]
use frame_support::storage::IterableStorageDoubleMap;

/// `Position` exactly as Phase 1 encoded it: no `funding_index`.
///
/// SCALE has no field delimiters, so this value is 32 bytes against the current 48. That also
/// means decoding a *current-format* value as this type would silently succeed and yield
/// garbage (the trailing 16 bytes are simply left unconsumed) — which is precisely why the
/// migration below is gated on the on-chain storage version instead of running unconditionally.
#[derive(Decode)]
struct PositionV0 {
    size_lots: i64,
    entry_price_ticks: PriceTicks,
    realized_pnl: SignedQuote,
}

/// Every `(key, value)` stored directly under `prefix`, collected **before** any write.
///
/// Writing while iterating a prefix can make the iterator pick up its own output, so every
/// migration here collects first and mutates afterwards.
fn collect_raw(prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut previous = prefix.to_vec();
    while let Some(next) = sp_io::storage::next_key(&previous).filter(|n| n.starts_with(prefix)) {
        previous = next.clone();
        if let Some(value) = unhashed::get_raw(&next) {
            out.push((next, value));
        }
    }
    out
}

/// Skips the 16-byte hash of a `Blake2_128Concat` hasher, decodes the key that follows, and
/// leaves `cursor` pointing at whatever comes next.
fn next_concat_key<'a, K: Decode>(cursor: &mut &'a [u8]) -> Option<K> {
    if cursor.len() < 16 {
        return None;
    }
    let mut rest = &cursor[16..];
    let key = K::decode(&mut rest).ok()?;
    *cursor = rest;
    Some(key)
}

pub mod v1 {
    use super::*;

    /// Adds `Position::funding_index` to every stored position.
    pub struct Migration<T>(core::marker::PhantomData<T>);

    impl<T: Config> Migration<T> {
        /// Funding was never charged in Phase 1, so an existing position starts from its
        /// market's *current* index.
        ///
        /// Seeding `0` instead would bill every open position for every epoch that elapsed
        /// before the upgrade — a retroactive charge users never agreed to.
        fn funding_index_for(market_id: MarketId) -> SignedQuote {
            Markets::<T>::get(market_id)
                .map(|m| m.funding_index)
                .unwrap_or(0)
        }
    }

    // `UncheckedOnRuntimeUpgrade` (not `OnRuntimeUpgrade`) because `VersionedMigration` performs
    // the version check on our behalf: it only calls this when the on-chain version is `0`.
    impl<T: Config> frame_support::traits::UncheckedOnRuntimeUpgrade for Migration<T> {
        fn on_runtime_upgrade() -> Weight {
            let mut translated = 0u64;

            // The v0 layout is a **two** component key `(AccountId, MarketId)`; `SubaccountId`
            // only arrives in v2. The pallet's current `Positions` is an NMap with three
            // components, so its accessors would try to split a two-part key into three and
            // silently drop every stored position. Decode the key by hand and rewrite the value
            // in place.
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
            for (key, raw) in collect_raw(&prefix) {
                let mut cursor = &key[prefix.len()..];
                let Some(_who) = next_concat_key::<T::AccountId>(&mut cursor) else {
                    continue;
                };
                let Some(market_id) = next_concat_key::<MarketId>(&mut cursor) else {
                    continue;
                };
                let Ok(old) = PositionV0::decode(&mut &raw[..]) else {
                    continue;
                };

                unhashed::put(
                    &key,
                    &Position {
                        size_lots: old.size_lots,
                        entry_price_ticks: old.entry_price_ticks,
                        realized_pnl: old.realized_pnl,
                        funding_index: Self::funding_index_for(market_id),
                    },
                );
                translated = translated.saturating_add(1);
            }

            // `BadDebt` is `ValueQuery`, so it is already `0` and needs no initialisation.
            //
            // `realized_pnl` is deliberately *not* folded here. It settles lazily on the next
            // touch, and every path that can move cash (`withdraw_collateral`, `place_order`)
            // now settles before it acts — so leaving it alone cannot be exploited. Folding it
            // during the migration would additionally risk draining accounts whose losses exceed
            // their collateral, for no correctness gain.

            T::DbWeight::get().reads_writes(translated.saturating_add(1), translated)
        }

        #[cfg(feature = "try-runtime")]
        fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
            // Count **keys**, never values.
            //
            // `StorageDoubleMap::iter()` decodes each value as the *current* `Position`. Before
            // the upgrade the stored bytes are still the old 32-byte encoding, so every entry
            // fails to decode and is silently skipped — the count would always come back 0, and
            // `post_upgrade` (where the values decode fine) would always disagree. Counting
            // undecodable entries is the whole point of a `pre_upgrade` check, so it has to go
            // Count the raw keys under the prefix. The stored keys are still the v0 two-part
            // layout, which the current three-part accessor cannot decode.
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
            let count = collect_raw(&prefix).len() as u64;
            Ok(count.encode())
        }

        #[cfg(feature = "try-runtime")]
        fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
            let before = u64::decode(&mut &state[..]).map_err(|_| "bad pre_upgrade state")?;
            // Same reasoning as `pre_upgrade`: count raw keys so the two numbers are comparable.
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
            let after = collect_raw(&prefix).len() as u64;
            if before != after {
                return Err("position count changed during migration".into());
            }
            for (key, raw) in collect_raw(&prefix) {
                let mut cursor = &key[prefix.len()..];
                let Some(_) = next_concat_key::<T::AccountId>(&mut cursor) else {
                    continue;
                };
                let Some(market_id) = next_concat_key::<MarketId>(&mut cursor) else {
                    continue;
                };
                let Ok(position) = Position::decode(&mut &raw[..]) else {
                    return Err("position did not decode after migration".into());
                };
                if position.funding_index != Self::funding_index_for(market_id) {
                    return Err("position not seeded to its market funding index".into());
                }
            }
            Ok(())
        }
    }
}

/// v1 -> v2: adds the `SubaccountId` dimension (§9.1).
///
/// The pallet and storage prefixes do **not** change, so pre-upgrade entries still live under
/// the same prefix with a shorter key — they simply cannot be found by the new accessors. Each
/// old key is therefore decoded, the value re-stored under `(…, DEFAULT_SUBACCOUNT, …)` and the
/// old key killed.
///
/// Every pass **collects first and writes afterwards**: the new keys sit under the prefix being
/// iterated, so writing during iteration would make the iterator revisit them.
pub mod v2 {
    use super::*;

    pub struct Migration<T>(core::marker::PhantomData<T>);

    impl<T: Config> Migration<T> {
        /// Every `(key, value)` stored directly under `prefix`.
        fn collect(prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut out = Vec::new();
            let mut previous = prefix.to_vec();
            while let Some(next) =
                sp_io::storage::next_key(&previous).filter(|n| n.starts_with(prefix))
            {
                previous = next.clone();
                if let Some(value) = unhashed::get_raw(&next) {
                    out.push((next, value));
                }
            }
            out
        }

        /// Skips the 16-byte hash of a `Blake2_128Concat` hasher and decodes the key that
        /// follows, returning the remaining bytes.
        fn next_concat_key<'a, K: Decode>(cursor: &mut &'a [u8]) -> Option<K> {
            if cursor.len() < 16 {
                return None;
            }
            let mut rest = &cursor[16..];
            let key = K::decode(&mut rest).ok()?;
            *cursor = rest;
            Some(key)
        }
    }

    impl<T: Config> frame_support::traits::UncheckedOnRuntimeUpgrade for Migration<T> {
        fn on_runtime_upgrade() -> Weight {
            let mut accounts = 0u64;
            let mut positions = 0u64;
            let mut orders = 0u64;

            // ---- Accounts: (AccountId) -> (AccountId, SubaccountId)
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Accounts");
            for (key, value) in Self::collect(&prefix) {
                let mut cursor = &key[prefix.len()..];
                let Some(who) = Self::next_concat_key::<T::AccountId>(&mut cursor) else {
                    continue;
                };
                let Some(account) = Account::decode(&mut &value[..]).ok() else {
                    continue;
                };
                unhashed::kill(&key);
                Accounts::<T>::insert(&who, DEFAULT_SUBACCOUNT, account);
                SubaccountIndex::<T>::mutate(&who, |subs| {
                    if !subs.contains(&DEFAULT_SUBACCOUNT) {
                        let _ = subs.try_push(DEFAULT_SUBACCOUNT);
                    }
                });
                accounts = accounts.saturating_add(1);
            }

            // ---- Positions: (AccountId, MarketId) -> (AccountId, SubaccountId, MarketId)
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
            for (key, value) in Self::collect(&prefix) {
                let mut cursor = &key[prefix.len()..];
                let Some(who) = Self::next_concat_key::<T::AccountId>(&mut cursor) else {
                    continue;
                };
                let Some(market_id) = Self::next_concat_key::<MarketId>(&mut cursor) else {
                    continue;
                };
                let Some(position) = Position::decode(&mut &value[..]).ok() else {
                    continue;
                };
                unhashed::kill(&key);
                Positions::<T>::insert((&who, DEFAULT_SUBACCOUNT, market_id), position);
                positions = positions.saturating_add(1);
            }

            // ---- UserOrderByClientId: (AccountId, ClientOrderId) -> (…, SubaccountId, …)
            let prefix =
                frame_support::storage::storage_prefix(b"ExchangeCore", b"UserOrderByClientId");
            for (key, value) in Self::collect(&prefix) {
                let mut cursor = &key[prefix.len()..];
                let Some(who) = Self::next_concat_key::<T::AccountId>(&mut cursor) else {
                    continue;
                };
                let Some(client) = Self::next_concat_key::<ClientOrderId>(&mut cursor) else {
                    continue;
                };
                let Some(ids) = <(MarketId, OrderId)>::decode(&mut &value[..]).ok() else {
                    continue;
                };
                unhashed::kill(&key);
                UserOrderByClientId::<T>::insert((&who, DEFAULT_SUBACCOUNT, client), ids);
                orders = orders.saturating_add(1);
            }

            T::DbWeight::get().reads_writes(
                (accounts + positions + orders).saturating_add(3),
                (accounts + positions + orders)
                    .saturating_mul(2)
                    .saturating_add(3),
            )
        }

        #[cfg(feature = "try-runtime")]
        fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
            // Count raw keys, not typed values: the stored values are still the old encoding,
            // so `Positions::iter()` would skip every entry and report 0.
            let prefix = frame_support::storage::storage_prefix(b"ExchangeCore", b"Positions");
            let count = Self::collect(&prefix).len() as u64;
            Ok(count.encode())
        }

        #[cfg(feature = "try-runtime")]
        fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
            let before = u64::decode(&mut &state[..]).map_err(|_| "bad pre_upgrade state")?;
            let after = Positions::<T>::iter_keys().count() as u64;
            if before != after {
                return Err("position count changed during subaccount migration".into());
            }
            Ok(())
        }
    }
}

pub mod v3 {
    use super::*;
    use frame_system::pallet_prelude::BlockNumberFor;

    /// The pre-v3 layout. It carried the Root-set mark price cache (`mark_price_ticks` and
    /// `mark_price_updated_at`) that `pallet-oracle` replaced; dropping them without a
    /// migration would make every stored market decode as garbage rather than fail loudly.
    #[derive(codec::Decode)]
    struct OldMarket<BlockNumber> {
        pub base_asset_code: [u8; 8],
        pub quote_asset_code: [u8; 8],
        pub tick_size_quote: QuoteAmount,
        pub lot_size_base: u64,
        pub base_precision: u64,
        pub initial_margin_ppm: MarginPpm,
        pub maintenance_margin_ppm: MarginPpm,
        pub liquidity_tier: u32,
        pub maker_fee_ppm: FeePpm,
        pub taker_fee_ppm: FeePpm,
        pub max_position_lots: Lots,
        pub status: MarketStatus,
        pub mark_price_ticks: PriceTicks,
        pub mark_price_updated_at: BlockNumber,
        pub best_bid: Option<PriceTicks>,
        pub best_ask: Option<PriceTicks>,
        pub bid_level_count: u32,
        pub ask_level_count: u32,
        pub net_oi_lots: i64,
        pub funding_index: SignedQuote,
        pub last_funding_at: BlockNumber,
        pub rate_ppm: i32,
        pub next_order_id: OrderId,
    }

    pub struct Migration<T>(core::marker::PhantomData<T>);

    impl<T: Config> frame_support::traits::UncheckedOnRuntimeUpgrade for Migration<T> {
        fn on_runtime_upgrade() -> Weight {
            let mut translated = 0u64;
            Markets::<T>::translate::<OldMarket<BlockNumberFor<T>>, _>(|_market_id, old| {
                translated = translated.saturating_add(1);
                Some(Market {
                    base_asset_code: old.base_asset_code,
                    quote_asset_code: old.quote_asset_code,
                    tick_size_quote: old.tick_size_quote,
                    lot_size_base: old.lot_size_base,
                    base_precision: old.base_precision,
                    initial_margin_ppm: old.initial_margin_ppm,
                    maintenance_margin_ppm: old.maintenance_margin_ppm,
                    liquidity_tier: old.liquidity_tier,
                    maker_fee_ppm: old.maker_fee_ppm,
                    taker_fee_ppm: old.taker_fee_ppm,
                    max_position_lots: old.max_position_lots,
                    status: old.status,
                    best_bid: old.best_bid,
                    best_ask: old.best_ask,
                    bid_level_count: old.bid_level_count,
                    ask_level_count: old.ask_level_count,
                    net_oi_lots: old.net_oi_lots,
                    funding_index: old.funding_index,
                    last_funding_at: old.last_funding_at,
                    rate_ppm: old.rate_ppm,
                    next_order_id: old.next_order_id,
                })
            });
            T::DbWeight::get().reads_writes(translated, translated)
        }
    }
}
