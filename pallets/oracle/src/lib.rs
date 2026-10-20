//! # Sunchain Oracle (Phase 2, P2-A)
//!
//! Multi-source price aggregation producing the **index price** of every tracked market.
//!
//! Design follows dYdX v4 `x/prices`:
//!
//! * Whitelisted reporters submit quotes; the chain only validates and aggregates them
//!   (dYdX runs this as a validator-side pricefeed daemon, later replaced by Slinky).
//! * Aggregation is a **median** over all *fresh* sources, not a weighted mean, so a single
//!   misbehaving source cannot move the price.
//! * A price is only written when it moves by more than `MinPriceChangePpm` (dYdX
//!   `MarketParam.MinPriceChangePpm`).
//! * There is **one** on-chain price per market. dYdX uses the index price directly for
//!   margin, unrealised PnL, funding and liquidation; it never uses the last traded price and
//!   it has no separate mark price. We mirror that: `mark_price == index_price`.
//! * When the quorum is not met we simply do **not** update. There is no automatic market
//!   status change; `is_fresh()` is the single gate the risk engine must consult.
//!
//! Phase 2 design refs: §5 (price layer), §6.1 (premium), §20 (open parameters).

#![cfg_attr(not(feature = "std"), no_std)]

pub use exchange_primitives::PriceOracle;
pub use pallet::*;
pub use types::{MarketId, PricePoint, PriceTicks, Quote, SourceRecord};

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;

pub mod types;
pub mod weights;

#[frame_support::pallet]
pub mod pallet {
	use super::types::{
		above_min_price_change, median, MarketId, PricePoint, PriceTicks, Quote, SourceRecord,
	};
	use exchange_primitives::PriceOracle;
	use sp_runtime::{
		traits::{IdentifyAccount, ValidateUnsigned, Verify},
		transaction_validity::{
			InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
			ValidTransaction,
		},
	};
	use frame_support::{pallet_prelude::*, traits::{BuildGenesisConfig, EnsureOrigin}};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::Saturating;
	use sp_std::vec::Vec;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Domain tag for vote signatures, so a signature made for a vote cannot be replayed as
	/// some other payload the same key signs.
	pub const VOTE_TAG: &[u8] = b"SUN-ORACLE-VOTE-V1";

	// ------------------------------------------------------------------ config

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Origin allowed to manage sources, markets and emergency overrides.
		type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Maximum number of whitelisted price sources.
		#[pallet::constant]
		type MaxSources: Get<u32>;

		/// Minimum number of *fresh* sources required to publish a price (dYdX
		/// `MinExchanges`).
		///
		/// **An availability mechanism, not an anti-manipulation one.** It tolerates a source
		/// being down, and it filters a *single* dissenting source. It does not protect
		/// against every source being fed by the same upstream venue: our sources run the
		/// same feed code against the same exchanges, so they agree by construction. The real
		/// protection against a bad venue is the cross-venue median computed off-chain,
		/// before any vote is made. See the worklog, 2026-09-18.
		#[pallet::constant]
		type MinSources: Get<u32>;

		/// Maximum number of markets the oracle aggregates in one block. Bounds
		/// `on_initialize`.
		#[pallet::constant]
		type MaxMarkets: Get<u32>;

		/// A quote older than this many blocks is ignored.
		#[pallet::constant]
		type MaxPriceAge: Get<BlockNumberFor<Self>>;

		/// Minimum relative move (ppm) before a price is rewritten (dYdX
		/// `MinPriceChangePpm`).
		#[pallet::constant]
		type MinPriceChangePpm: Get<u32>;

		type WeightInfo: WeightInfo;

		/// Signature a whitelisted source uses to authenticate an injected vote.
		///
		/// The vote arrives as an **unsigned** extrinsic — dYdX v4 injects price updates into
		/// the block rather than sending them as user transactions, so there is no fee, no
		/// nonce and no balance to run out of. That removes the signer the extrinsic would
		/// normally provide, so the source signs *inside* the call instead.
		type Signature: Verify<Signer = Self::Signer> + Parameter;

		/// Public key of a source. Converts into [`Config::AccountId`], which is what the
		/// whitelist is keyed by.
		type Signer: IdentifyAccount<AccountId = Self::AccountId>;
	}

	// ------------------------------------------------------------------ storage

	/// Whitelisted price sources.
	#[pallet::storage]
	pub type Sources<T: Config> = StorageValue<
		_,
		BoundedVec<SourceRecord<T::AccountId, BlockNumberFor<T>>, T::MaxSources>,
		ValueQuery,
	>;

	/// Latest quote per `(market, source)`.
	#[pallet::storage]
	pub type Quotes<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		MarketId,
		Blake2_128Concat,
		T::AccountId,
		Quote<BlockNumberFor<T>>,
		OptionQuery,
	>;

	/// Aggregated on-chain price per market.
	#[pallet::storage]
	pub type MarketPrices<T: Config> =
		StorageMap<_, Blake2_128Concat, MarketId, PricePoint<BlockNumberFor<T>>, OptionQuery>;

	/// Markets aggregated by `on_initialize`. Bounded so the hook cannot grow without limit.
	#[pallet::storage]
	pub type TrackedMarkets<T: Config> =
		StorageValue<_, BoundedVec<MarketId, T::MaxMarkets>, ValueQuery>;

	// ------------------------------------------------------------------ events

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A source published new quotes.
		QuotesSubmitted { who: T::AccountId, count: u32 },
		/// The aggregated price of a market moved.
		IndexPriceUpdated { market_id: MarketId, price_ticks: PriceTicks, source_count: u32 },
		/// Governance forced a price. Must be audited; never used in normal operation.
		ForcePriceOverride { market_id: MarketId, price_ticks: PriceTicks },
		/// A market was added to / removed from the aggregation set.
		MarketTracked { market_id: MarketId },
		MarketUntracked { market_id: MarketId },
		/// A source was enabled or disabled.
		SourceUpdated { who: T::AccountId, enabled: bool },
	}

	// ------------------------------------------------------------------ errors

	#[pallet::error]
	pub enum Error<T> {
		/// The caller is not a whitelisted, enabled price source.
		SourceNotWhitelisted,
		TooManySources,
		TooManyMarkets,
		InvalidPrice,
		MarketNotTracked,
		MarketAlreadyTracked,
		/// Nothing was written because the quorum was not met or the move was below
		/// `MinPriceChangePpm`.
		NoPriceUpdate,
		/// A vote whose nonce does not move forward. Replay protection for the unsigned path,
		/// which has no extrinsic nonce of its own.
		StaleVote,
		/// The inner signature does not match the vote it claims to sign.
		BadVoteSignature,
	}

	// ------------------------------------------------------------------ hooks

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(now: BlockNumberFor<T>) -> Weight {
			let markets = TrackedMarkets::<T>::get();
			for market_id in markets.iter() {
				Self::aggregate(*market_id, now);
			}
			T::WeightInfo::on_initialize(markets.len() as u32, Sources::<T>::get().len() as u32)
		}
	}

	// ------------------------------------------------------------------ genesis

	/// An external price feed has to be whitelisted before it can publish anything, and this
	/// cannot be done retroactively: a brand new chain has **no** sources and therefore no
	/// prices at all until governance calls `set_source`. Configuring them here means the
	/// index price exists from block one.
	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		/// Accounts allowed to publish quotes from the first block.
		pub sources: Vec<T::AccountId>,
		/// Markets aggregated from the first block. A market that is not tracked is never
		/// aggregated, no matter how many quotes arrive for it.
		pub tracked_markets: Vec<MarketId>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			let mut sources: BoundedVec<SourceRecord<T::AccountId, BlockNumberFor<T>>, T::MaxSources> =
				BoundedVec::new();
			for key in self.sources.iter() {
				let record = SourceRecord {
					key: key.clone(),
					enabled: true,
					last_submitted_at: Default::default(),
					last_nonce: 0,
				};
				// A genesis with more sources than `MaxSources` is a broken spec, and saying so
				// at genesis is far better than running with a silently truncated whitelist.
				sources.try_push(record).expect("genesis sources exceed MaxSources");
			}
			Sources::<T>::put(sources);

			let mut markets: BoundedVec<MarketId, T::MaxMarkets> = BoundedVec::new();
			for market in self.tracked_markets.iter() {
				markets.try_push(*market).expect("genesis tracked markets exceed MaxMarkets");
			}
			TrackedMarkets::<T>::put(markets);
		}
	}

	// ------------------------------------------------------------------ calls

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Publishes one quote per market for the calling source.
		///
		/// Quotes are only recorded here; the actual aggregation happens in `on_initialize`,
		/// so the cost of this call does not depend on the number of markets a block has to
		/// process.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::submit_prices(prices.len() as u32, Sources::<T>::get().len() as u32))]
		pub fn submit_prices(
			origin: OriginFor<T>,
			prices: BoundedVec<(MarketId, PriceTicks), T::MaxMarkets>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let now = Self::now();

			let nonce = {
				let sources = Sources::<T>::get();
				let record = sources
					.iter()
					.find(|s| s.key == who && s.enabled)
					.ok_or(Error::<T>::SourceNotWhitelisted)?;
				record.last_nonce.saturating_add(1)
			};

			for (market_id, price) in prices.iter() {
				ensure!(*price > 0, Error::<T>::InvalidPrice);
				Quotes::<T>::insert(
					market_id,
					&who,
					Quote { price_ticks: *price, at: now, nonce },
				);
			}

			Sources::<T>::try_mutate(|sources| -> DispatchResult {
				let record = sources
					.iter_mut()
					.find(|s| s.key == who)
					.ok_or(Error::<T>::SourceNotWhitelisted)?;
				record.last_nonce = nonce;
				record.last_submitted_at = now;
				Ok(())
			})?;

			Self::deposit_event(Event::QuotesSubmitted {
				who,
				count: prices.len() as u32,
			});
			Ok(())
		}

		/// Adds or disables a price source. Governance only.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::set_source())]
		pub fn set_source(
			origin: OriginFor<T>,
			who: T::AccountId,
			enabled: bool,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			let now = Self::now();

			Sources::<T>::try_mutate(|sources| -> DispatchResult {
				match sources.iter_mut().find(|s| s.key == who) {
					Some(record) => record.enabled = enabled,
					None => {
						ensure!(enabled, Error::<T>::SourceNotWhitelisted);
						sources
							.try_push(SourceRecord {
								key: who.clone(),
								enabled: true,
								last_submitted_at: now,
								last_nonce: 0,
							})
							.map_err(|_| Error::<T>::TooManySources)?;
					},
				}
				Ok(())
			})?;

			Self::deposit_event(Event::SourceUpdated { who, enabled });
			Ok(())
		}

		/// Starts / stops aggregating a market. Governance only.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::track_market())]
		pub fn track_market(
			origin: OriginFor<T>,
			market_id: MarketId,
			tracked: bool,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			TrackedMarkets::<T>::try_mutate(|markets| -> DispatchResult {
				let existing = markets.iter().position(|m| *m == market_id);
				match (existing, tracked) {
					(Some(_), false) => {
						markets.retain(|m| *m != market_id);
						Self::deposit_event(Event::MarketUntracked { market_id });
					},
					(None, true) => {
						markets
							.try_push(market_id)
							.map_err(|_| Error::<T>::TooManyMarkets)?;
						Self::deposit_event(Event::MarketTracked { market_id });
					},
					(Some(_), true) => return Err(Error::<T>::MarketAlreadyTracked.into()),
					(None, false) => return Err(Error::<T>::MarketNotTracked.into()),
				}
				Ok(())
			})
		}

		/// Emergency price override. Emits a dedicated event so it is always auditable.
		///
		/// dYdX v4 has no equivalent; it exists here only because a stale quorum must not be
		/// able to freeze a market forever. Must be restricted to a multisig origin.
		/// Publishes quotes as an **injected** vote (dYdX v4 `MsgUpdateMarketPrices`).
		///
		/// Unsigned at the extrinsic level so it costs nothing, needs no balance and does not
		/// queue behind other transactions — a feed whose account ran out of money would stop
		/// silently, which is the worst possible failure for a price. Identity comes from the
		/// inner signature, checked against the whitelist, and `nonce` must move forward.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::submit_prices(prices.len() as u32, Sources::<T>::get().len() as u32))]
		pub fn submit_price_vote(
			origin: OriginFor<T>,
			source: T::AccountId,
			signature: T::Signature,
			prices: BoundedVec<(MarketId, PriceTicks), T::MaxMarkets>,
			nonce: u64,
		) -> DispatchResult {
			ensure_none(origin)?;
			let who = source.clone();
			let now = Self::now();

			let record = Sources::<T>::get()
				.into_iter()
				.find(|s| s.key == who && s.enabled)
				.ok_or(Error::<T>::SourceNotWhitelisted)?;
			ensure!(nonce > record.last_nonce, Error::<T>::StaleVote);
			let payload = Self::vote_payload(&source, &prices, nonce);
			ensure!(signature.verify(&payload[..], &who), Error::<T>::BadVoteSignature);

			for (market_id, price) in prices.iter() {
				ensure!(*price > 0, Error::<T>::InvalidPrice);
				Quotes::<T>::insert(market_id, &who, Quote { price_ticks: *price, at: now, nonce });
			}

			Sources::<T>::try_mutate(|sources| -> DispatchResult {
				let record = sources
					.iter_mut()
					.find(|s| s.key == who)
					.ok_or(Error::<T>::SourceNotWhitelisted)?;
				record.last_nonce = nonce;
				record.last_submitted_at = now;
				Ok(())
			})?;

			Self::deposit_event(Event::QuotesSubmitted { who, count: prices.len() as u32 });
			Ok(())
		}

		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::force_set_price())]
		pub fn force_set_price(
			origin: OriginFor<T>,
			market_id: MarketId,
			price_ticks: PriceTicks,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			ensure!(price_ticks > 0, Error::<T>::InvalidPrice);
			let now = Self::now();
			MarketPrices::<T>::insert(
				market_id,
				PricePoint { price_ticks, updated_at: now, source_count: 0 },
			);
			Self::deposit_event(Event::ForcePriceOverride { market_id, price_ticks });
			Ok(())
		}
	}

	// ------------------------------------------------------------------ internals

	impl<T: Config> Pallet<T> {
		fn now() -> BlockNumberFor<T> {
			frame_system::Pallet::<T>::block_number()
		}

		/// What a source signs. Exposed so the off-chain feed builds exactly the same bytes
		/// rather than re-deriving the layout — a mismatch would produce votes that are simply
		/// rejected, with nothing to look at.
		pub fn vote_payload(
			source: &T::AccountId,
			prices: &[(MarketId, PriceTicks)],
			nonce: u64,
		) -> Vec<u8> {
			(VOTE_TAG, source.clone(), prices.to_vec(), nonce).encode()
		}

		/// Collects every *fresh* quote for `market` and returns their median.
		///
		/// Returns `None` when fewer than `MinSources` sources have a quote newer than
		/// `MaxPriceAge`. Iteration is bounded by `MaxSources`.
		pub fn aggregate(market_id: MarketId, now: BlockNumberFor<T>) -> Option<PriceTicks> {
			let max_age = T::MaxPriceAge::get();
			let mut prices: Vec<PriceTicks> = Vec::new();

			for (_, quote) in Quotes::<T>::iter_prefix(market_id) {
				if now.saturating_sub(quote.at) <= max_age {
					prices.push(quote.price_ticks);
				}
				if prices.len() as u32 >= T::MaxSources::get() {
					break
				}
			}

			if (prices.len() as u32) < T::MinSources::get() {
				return None
			}

			let new_price = median(&mut prices)?;
			let source_count = prices.len() as u32;
			let previous = MarketPrices::<T>::get(market_id);

			// dYdX `MinPriceChangePpm`: ignore moves that are too small to matter.
			let moved = previous
				.map(|p| {
					above_min_price_change(p.price_ticks, new_price, T::MinPriceChangePpm::get())
				})
				.unwrap_or(true);

			if !moved {
				// The value did not move enough to be worth writing, but the sources are still
				// answering, and freshness is a different question from value. Returning here
				// without touching `updated_at` left a price that never moved to age past
				// `MaxPriceAge`: the market became permanently untradeable while its
				// publishers kept publishing, with nothing anywhere saying why.
				if let Some(point) = previous {
					MarketPrices::<T>::insert(
						market_id,
						PricePoint { updated_at: now, source_count, ..point },
					);
				}
				return Some(new_price)
			}

			let point = PricePoint { price_ticks: new_price, updated_at: now, source_count };
			MarketPrices::<T>::insert(market_id, point);
			Self::deposit_event(Event::IndexPriceUpdated {
				market_id,
				price_ticks: new_price,
				source_count,
			});
			Some(new_price)
		}

		/// The stored price together with its age, used to decide freshness.
		fn stored(market_id: MarketId) -> Option<(PricePoint<BlockNumberFor<T>>, BlockNumberFor<T>)> {
			let point = MarketPrices::<T>::get(market_id)?;
			let age = Self::now().saturating_sub(point.updated_at);
			Some((point, age))
		}
	}

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			let Call::submit_price_vote { source: who, signature, prices, nonce } = call else {
				return InvalidTransaction::Call.into()
			};

			// Unlike the exchange batch, this one *is* allowed to arrive from the network:
			// the feed that produces it is not necessarily the node that authors the next
			// block, so the vote has to be able to travel. Its authenticity comes from the
			// inner signature, not from where it came from.
			let _ = source;

			let record = match Sources::<T>::get().into_iter().find(|s| s.key == *who && s.enabled) {
				Some(r) => r,
				None => return InvalidTransaction::BadSigner.into(),
			};
			if *nonce <= record.last_nonce {
				return InvalidTransaction::Stale.into()
			}
			if prices.is_empty() || prices.len() as u32 > T::MaxMarkets::get() {
				return InvalidTransaction::Call.into()
			}
			let payload = Self::vote_payload(who, prices, *nonce);
			if !signature.verify(&payload[..], who) {
				return InvalidTransaction::BadProof.into()
			}

			// One live vote per (source, nonce) — a source cannot fill the pool.
			ValidTransaction::with_tag_prefix("OracleVote")
				.priority(TransactionPriority::MAX / 2)
				.longevity(64)
				.propagate(true)
				.and_provides((b"vote", who.clone(), *nonce))
				.build()
		}
	}

	impl<T: Config> PriceOracle<MarketId> for Pallet<T> {
		fn index_price(market_id: MarketId) -> Option<PriceTicks> {
			let (point, age) = Self::stored(market_id)?;
			(age <= T::MaxPriceAge::get()).then_some(point.price_ticks)
		}

		/// Equals [`Self::index_price`]: dYdX uses a single price. The type-level separation
		/// is kept so clamping/smoothing can be introduced later without touching call sites.
		fn mark_price(market_id: MarketId) -> Option<PriceTicks> {
			Self::index_price(market_id)
		}

		fn is_fresh(market_id: MarketId) -> bool {
			Self::index_price(market_id).is_some()
		}
	}

	// ------------------------------------------------------------------ weights

	pub trait WeightInfo {
		/// `prices` is how many markets are quoted, `sources` how many are registered.
		fn submit_prices(prices: u32, sources: u32) -> Weight;
		fn set_source() -> Weight;
		fn track_market() -> Weight;
		fn force_set_price() -> Weight;
		fn on_initialize(markets: u32, sources: u32) -> Weight;
	}

	/// Placeholder weights. Replaced by benchmark output before this pallet is used in
	/// production (same rule as Phase 1 design §11.1).
	impl<T: Config> WeightInfo for Pallet<T> {
		fn submit_prices(_prices: u32, _sources: u32) -> Weight {
			Weight::from_parts(50_000_000, 0)
		}
		fn set_source() -> Weight {
			Weight::from_parts(20_000_000, 0)
		}
		fn track_market() -> Weight {
			Weight::from_parts(20_000_000, 0)
		}
		fn force_set_price() -> Weight {
			Weight::from_parts(20_000_000, 0)
		}
		fn on_initialize(_markets: u32, _sources: u32) -> Weight {
			Weight::from_parts(50_000_000, 0)
		}
	}
}

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;
