//! Tests for the path an external price feed actually takes:
//! `set_source` → `submit_prices` → `on_initialize` → `aggregate` → `MarketPrices`.

use crate::{Error, Event, MarketPrices, Quotes, Sources};
use exchange_primitives::{MarketId, PriceOracle, PriceTicks};
use frame_support::{assert_noop, assert_ok, traits::Hooks};

use crate::mock::*;

const BTC: MarketId = 1;

const ALICE: AccountId = 1;
const BOB: AccountId = 2;
const CHARLIE: AccountId = 3;

fn root() -> frame_system::pallet_prelude::OriginFor<Test> {
	frame_system::RawOrigin::Root.into()
}

fn whitelist(accounts: &[AccountId]) {
	for a in accounts {
		assert_ok!(crate::Pallet::<Test>::set_source(root(), *a, true));
	}
}

fn track(market: MarketId) {
	assert_ok!(crate::Pallet::<Test>::track_market(root(), market, true));
}

fn submit(who: AccountId, prices: &[(MarketId, PriceTicks)]) -> Result<(), sp_runtime::DispatchError> {
	crate::Pallet::<Test>::submit_prices(
		frame_system::RawOrigin::Signed(who).into(),
		prices.to_vec().try_into().expect("within MaxMarkets"),
	)
}

/// `on_initialize` is what actually aggregates.
fn aggregate() {
	let now = System::block_number();
	<crate::Pallet<Test> as Hooks<u64>>::on_initialize(now);
}

fn advance(blocks: u64) {
	System::set_block_number(System::block_number() + blocks);
}

#[test]
fn an_account_that_is_not_a_source_cannot_publish() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE]);
		assert_noop!(submit(BOB, &[(BTC, 60_000)]), Error::<Test>::SourceNotWhitelisted);
	});
}

#[test]
fn a_disabled_source_cannot_publish() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE]);
		assert_ok!(crate::Pallet::<Test>::set_source(root(), ALICE, false));
		assert_noop!(
			submit(ALICE, &[(BTC, 60_000)]),
			Error::<Test>::SourceNotWhitelisted
		);
	});
}

#[test]
fn disabling_an_unknown_source_is_rejected() {
	new_test_ext().execute_with(|| {
		// There is no record to disable; unchecked this would quietly create one.
		assert_noop!(
			crate::Pallet::<Test>::set_source(root(), ALICE, false),
			Error::<Test>::SourceNotWhitelisted
		);
		assert!(Sources::<Test>::get().is_empty());
	});
}

#[test]
fn a_zero_price_is_rejected() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE]);
		assert_noop!(submit(ALICE, &[(BTC, 0)]), Error::<Test>::InvalidPrice);
	});
}

/// Fewer fresh sources than `MinSources` means no price at all — never a price built from
/// too few reporters.
#[test]
fn below_quorum_no_price_is_published() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB, CHARLIE]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		aggregate();
		assert!(MarketPrices::<Test>::get(BTC).is_none(), "one source must not publish");

		// Positive control. The `None` above is only meaningful because one more source *does*
		// publish — without this the test would also pass if the hook never ran at all.
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_000);
	});
}

#[test]
fn a_stale_quote_stops_counting_toward_the_quorum() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB, CHARLIE]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_100)]));
		advance(MaxPriceAge::get() + 1);
		assert_ok!(submit(CHARLIE, &[(BTC, 60_200)]));
		aggregate();
		assert!(MarketPrices::<Test>::get(BTC).is_none(), "the stale two must not count");

		// Positive control: refresh ALICE and the quorum is met again, proving the price was
		// absent because of staleness and not because aggregation silently stopped.
		assert_ok!(submit(ALICE, &[(BTC, 60_200)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_200);
	});
}

/// The reason the summary is a median rather than a mean.
#[test]
fn one_outlier_source_does_not_move_the_price() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB, CHARLIE]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_100)]));
		assert_ok!(submit(CHARLIE, &[(BTC, 600_000)]));
		aggregate();
		let price = MarketPrices::<Test>::get(BTC).unwrap();
		assert_eq!(price.price_ticks, 60_100);
		assert_eq!(price.source_count, 3);
	});
}

/// Freshness is not the same question as value.
///
/// A price that never moves must stay usable: skipping the write *without* carrying
/// `updated_at` forward left a market permanently stale while its publishers kept publishing
/// on schedule — no error, no event, just every order refused inside a batch that reports
/// success. The value here stays untouched; only its age is refreshed.
#[test]
fn a_price_that_does_not_move_stays_fresh() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_000);

		// Same price again, well inside `MaxPriceAge`, below the change gate.
		advance(MaxPriceAge::get() - 1);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();

		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_000, "value unchanged");
		assert_eq!(
			MarketPrices::<Test>::get(BTC).unwrap().updated_at,
			System::block_number(),
			"the price must be re-dated, or it ages out of MaxPriceAge forever"
		);
		assert_eq!(crate::Pallet::<Test>::index_price(BTC), Some(60_000));

		// Negative control: stop publishing and it does go stale, so the assertion above is
		// about re-dating rather than about freshness always answering yes.
		advance(MaxPriceAge::get() + 1);
		assert_eq!(crate::Pallet::<Test>::index_price(BTC), None);
	});
}

/// `MinPriceChangePpm`: noise must not rewrite state, or every block re-proves the price.
#[test]
fn a_trivial_move_does_not_rewrite_the_price() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_000);

		// 0.17% — below the 1% gate.
		advance(1);
		assert_ok!(submit(ALICE, &[(BTC, 60_100)]));
		assert_ok!(submit(BOB, &[(BTC, 60_100)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 60_000);

		// 10% — the gate opens.
		advance(1);
		assert_ok!(submit(ALICE, &[(BTC, 66_000)]));
		assert_ok!(submit(BOB, &[(BTC, 66_000)]));
		aggregate();
		assert_eq!(MarketPrices::<Test>::get(BTC).unwrap().price_ticks, 66_000);
	});
}

/// Consumers reject quotes whose nonce has not advanced, so it must be monotonic per
/// source. This is the pallet's side of replay protection.
#[test]
fn the_nonce_advances_on_every_submission() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE]);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		let first = Quotes::<Test>::get(BTC, ALICE).unwrap().nonce;
		advance(1);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		let second = Quotes::<Test>::get(BTC, ALICE).unwrap().nonce;
		assert_eq!(second, first + 1);
	});
}

#[test]
fn a_published_price_is_readable_until_it_goes_stale() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();
		assert_eq!(
			<crate::Pallet<Test> as PriceOracle<MarketId>>::index_price(BTC),
			Some(60_000)
		);

		advance(MaxPriceAge::get() + 1);
		assert_eq!(<crate::Pallet<Test> as PriceOracle<MarketId>>::index_price(BTC), None);
	});
}

#[test]
fn publishing_emits_an_event() {
	new_test_ext().execute_with(|| {
		whitelist(&[ALICE, BOB]);
		track(BTC);
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		System::assert_has_event(Event::QuotesSubmitted { who: ALICE, count: 1 }.into());
	});
}

/// A brand new chain has no sources at all, so nothing can publish until governance acts.
/// Configuring them at genesis is what makes the index price exist from block one.
#[test]
fn genesis_config_whitelists_sources_and_tracks_markets() {
	new_test_ext().execute_with(|| {
		// Default genesis: nobody can publish and nothing is aggregated.
		assert!(Sources::<Test>::get().is_empty());
		assert!(crate::TrackedMarkets::<Test>::get().is_empty());
	});

	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		assert_eq!(Sources::<Test>::get().len(), 2);
		assert_eq!(crate::TrackedMarkets::<Test>::get().into_inner(), vec![BTC]);
		// Nothing further to do: those sources can publish immediately.
		assert_ok!(submit(ALICE, &[(BTC, 60_000)]));
		assert_ok!(submit(BOB, &[(BTC, 60_000)]));
		aggregate();
		assert_eq!(
			<crate::Pallet<Test> as PriceOracle<MarketId>>::index_price(BTC),
			Some(60_000)
		);
	});
}

// -------------------------------------------------------------------------------- injected
// votes (dYdX v4 `MsgUpdateMarketPrices`): unsigned at the extrinsic level, authenticated
// inside the call.

use crate::mock::{MockSigner, MockSignature};

fn vote(
	source: AccountId,
	prices: &[(MarketId, PriceTicks)],
	nonce: u64,
) -> Result<(), sp_runtime::DispatchError> {
	let signer = MockSigner(source);
	let bounded: Vec<_> = prices.to_vec();
	let payload = crate::Pallet::<Test>::vote_payload(&source, &bounded, nonce);
	crate::Pallet::<Test>::submit_price_vote(
		frame_system::RawOrigin::None.into(),
		source,
		MockSignature(signer.sign(&payload).0),
		bounded.try_into().expect("within MaxMarkets"),
		nonce,
	)
}

#[test]
fn an_injected_vote_publishes_quotes() {
	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		assert_ok!(vote(ALICE, &[(BTC, 60_000)], 1));
		assert_ok!(vote(BOB, &[(BTC, 60_100)], 1));
		aggregate();
		assert_eq!(
			<crate::Pallet<Test> as PriceOracle<MarketId>>::index_price(BTC),
			Some(60_050)
		);
	});
}

#[test]
fn a_vote_from_a_non_source_is_refused() {
	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		assert_noop!(vote(CHARLIE, &[(BTC, 60_000)], 1), Error::<Test>::SourceNotWhitelisted);
	});
}

/// The signature covers the prices: change them and the same signature no longer verifies.
#[test]
fn a_vote_signed_for_other_prices_is_refused() {
	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		let signer = MockSigner(ALICE);
		let payload = crate::Pallet::<Test>::vote_payload(&ALICE, &[(BTC, 60_000)], 1);
		let signature = MockSignature(signer.sign(&payload).0);
		assert_noop!(
			crate::Pallet::<Test>::submit_price_vote(
				frame_system::RawOrigin::None.into(),
				ALICE,
				signature,
				vec![(BTC, 999_999)].try_into().unwrap(),
				1,
			),
			Error::<Test>::BadVoteSignature
		);
	});
}

/// Replay protection. An unsigned extrinsic has no nonce of its own, so the vote carries one.
#[test]
fn a_vote_cannot_be_replayed() {
	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		assert_ok!(vote(ALICE, &[(BTC, 60_000)], 1));
		assert_noop!(vote(ALICE, &[(BTC, 60_000)], 1), Error::<Test>::StaleVote);
		// Equal, not just lower: a replay uses the very same nonce.
		assert_noop!(vote(ALICE, &[(BTC, 60_000)], 0), Error::<Test>::StaleVote);
		assert_ok!(vote(ALICE, &[(BTC, 60_000)], 2));
	});
}

#[test]
fn a_zero_price_cannot_be_voted() {
	new_test_ext_with(vec![ALICE, BOB], vec![BTC]).execute_with(|| {
		assert_noop!(vote(ALICE, &[(BTC, 0)], 1), Error::<Test>::InvalidPrice);
	});
}
