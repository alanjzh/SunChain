//! Permissionless liquidation and ADL entry points (Phase 2 §8).
//!
//! This pallet owns **no risk logic**. It is the thin, metered, publicly callable front door:
//! it forwards to `ExchangeEngine`, pays the keeper and emits the audit trail. Every margin,
//! PnL and matching decision stays in `pallet-exchange-core`, which is the only place allowed
//! to move user collateral.
//!
//! The dependency arrow is strictly one-way — the reverse call is forbidden by design §8.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;
pub mod selection;
pub mod weights;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use crate::weights::WeightInfo;
	use exchange_primitives::{
		ExchangeEngine, MarketId, PriceTicks, QuoteAmount, PPM_DENOM,
	};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The engine that owns positions, collateral and every risk decision.
		type Engine: ExchangeEngine<Self::AccountId>;

		/// Upper bound on liquidations / ADL targets one call may touch. Bounds the weight.
		type MaxLiquidationsPerCall: Get<u32>;

		/// Minimum return on equity (ppm) before an account may be ADL'd (§8.5).
		type MinAdlRoePpm: Get<i64>;

		/// How far the ADL execution price is shifted in the ADL'd account's favour, as a
		/// fraction of the adjusted mark (§8.5.1 §1).
		type AdlPriceBandPpm: Get<u32>;

		/// Weight info for this pallet's calls.
		type WeightInfo: weights::WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// `keeper` liquidated `who` in `market`, closing `lots`.
		Liquidated {
			keeper: T::AccountId,
			who: T::AccountId,
			market_id: MarketId,
			lots: exchange_primitives::Lots,
			keeper_rebate: QuoteAmount,
		},
		/// `targets` profitable accounts were force-reduced in `market` (§8.5).
		///
		/// §8.5.1: this is an exposure reduction, **not** a loss allocation. No account was
		/// charged a penalty, and no residual loss was cleared by this call.
		AutoDeleveraged {
			keeper: T::AccountId,
			market_id: MarketId,
			targets: u32,
			lots: exchange_primitives::Lots,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Nothing was closed: the account is healthy, or the book could not absorb it inside
		/// the allowed slippage band.
		NothingLiquidated,
		/// ADL was requested for an account with no recorded residual loss.
		NoResidualLoss,
		/// None of the proposed candidates qualified for ADL.
		NoAdlCandidates,
		ArithmeticOverflow,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Liquidates `who` in `market`. Permissionless: anyone may submit it, and the caller
		/// receives the accrued keeper rebate.
		///
		/// `lots == 0` lets the engine size a partial liquidation (§8.2); otherwise at most
		/// `lots` are closed.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::liquidate())]
		pub fn liquidate(
			origin: OriginFor<T>,
			who: T::AccountId,
			market_id: MarketId,
			lots: exchange_primitives::Lots,
		) -> DispatchResult {
			let keeper = ensure_signed(origin)?;

			// Every rejection reason (not underwater, stale price, no book depth inside the
			// slippage band) is a legitimate outcome for a keeper racing other keepers, so it
			// surfaces as an error rather than silently succeeding.
			let closed = T::Engine::liquidate_account(&keeper, &who, market_id, lots)?;
			ensure!(closed > 0, Error::<T>::NothingLiquidated);

			// Pay whatever this liquidation accrued. A keeper can never be paid more than the
			// penalty actually charged, so this cannot come out of user collateral.
			let rebate = T::Engine::pay_keeper(&keeper)?;

			Self::deposit_event(Event::Liquidated {
				keeper,
				who,
				market_id,
				lots: closed,
				keeper_rebate: rebate,
			});
			Ok(())
		}

		/// Force-reduces profitable opposite-side accounts when `bankrupt`'s position could not
		/// be closed normally (§8.5).
		///
		/// §8.5.1 governs this call: ADL **reduces exposure and never allocates loss**. Each
		/// account is settled at a price at least as good as the adjusted mark *for them*, the
		/// order comes from [`selection::select_adl_targets`] rather than the keeper, and the
		/// recorded residual loss is left untouched — only the insurance fund or governance may
		/// reduce it.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::adl(T::MaxLiquidationsPerCall::get()))]
		pub fn adl(
			origin: OriginFor<T>,
			bankrupt: T::AccountId,
			candidates: BoundedVec<T::AccountId, T::MaxLiquidationsPerCall>,
		) -> DispatchResult {
			let keeper = ensure_signed(origin)?;

			// ADL only responds to a loss that collateral, liquidation and the insurance fund
			// all failed to cover. Without one there is no exposure worth forcing anyone out of.
			let loss = T::Engine::residual_loss_of(&bankrupt);
			// ADL may shed exposure worth the uncovered loss — no more, and exactly once.
			// Without this a keeper could replay the call every block for as long as the loss
			// stays recorded, which is forever.
			let budget = loss.amount.saturating_sub(loss.shed_notional);
			ensure!(budget > 0, Error::<T>::NoResidualLoss);

			let targets = selection::select_adl_targets(
				candidates.into_iter(),
				|who| T::Engine::adl_rank(who, loss.market_id),
				loss.side_sign,
				T::MinAdlRoePpm::get(),
				T::MaxLiquidationsPerCall::get() as usize,
			);
			ensure!(!targets.is_empty(), Error::<T>::NoAdlCandidates);

			// Shed exposure worth roughly the uncovered loss — no more.
			//
			// §8.5.1: ADL is not a loss-allocation mechanism, so this is *not* "take the
			// shortfall out of these accounts". It is an explicit, auditable rule for how much
			// exposure to remove: keep reducing until the notional removed reaches the loss
			// that collateral, liquidation and the insurance fund all failed to cover.
			let mut remaining: QuoteAmount = budget;
			let mut total_lots: exchange_primitives::Lots = 0;
			let mut shed_notional: exchange_primitives::QuoteAmount = 0;
			for target in &targets {
				if remaining == 0 {
					break
				}
				let rank = T::Engine::adl_rank(target, loss.market_id)
					.ok_or(Error::<T>::NoAdlCandidates)?;
				let price = Self::adl_execution_price(
					rank.mark_price_ticks,
					rank.size_lots > 0,
					T::AdlPriceBandPpm::get(),
				)?;

				let per_lot = rank.notional_per_lot.max(1);
				let wanted = remaining.div_ceil(per_lot); // ceil: never shed less, no overflow
				// `wanted` is bounded by the position size, so the narrowing cast is safe.
				let lots = wanted.min(rank.size_lots.unsigned_abs() as QuoteAmount)
					as exchange_primitives::Lots;
				if lots == 0 {
					continue
				}
				T::Engine::adl_close(target, loss.market_id, lots, price)?;

				total_lots = total_lots.saturating_add(lots);
				shed_notional = shed_notional.saturating_add(
					(lots as exchange_primitives::QuoteAmount).saturating_mul(per_lot),
				);
				remaining = remaining
					.saturating_sub((lots as QuoteAmount).saturating_mul(per_lot));
			}

			// §8.5.1: ADL removes exposure worth the uncovered loss — no more. Folding the shed
			// notional back into the record is what makes it self-limiting; without it a keeper
			// could replay this every block for as long as the loss stays non-zero.
			T::Engine::note_adl_shed(&bankrupt, shed_notional);

			Self::deposit_event(Event::AutoDeleveraged {
				keeper,
				market_id: loss.market_id,
				targets: targets.len() as u32,
				lots: total_lots,
			});
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// §8.5.1 §1: the ADL execution price, shifted **in the ADL'd account's favour**.
		///
		/// A long is reduced by selling, so it is paid *more* than the adjusted mark; a short is
		/// reduced by buying, so it pays *less*. Rounding goes the same way — the band is always
		/// rounded up, so the account can only ever come out ahead of the adjusted mark.
		pub fn adl_execution_price(
			mark: PriceTicks,
			target_is_long: bool,
			band_ppm: u32,
		) -> Result<PriceTicks, DispatchError> {
			let scaled = (mark as u128)
				.checked_mul(band_ppm as u128)
				.ok_or(Error::<T>::ArithmeticOverflow)?;
			let band = (scaled + PPM_DENOM as u128 - 1) / PPM_DENOM as u128;

			let price = if target_is_long {
				(mark as u128).checked_add(band)
			} else {
				Some((mark as u128).saturating_sub(band))
			}
			.ok_or(Error::<T>::ArithmeticOverflow)?;

			Ok(u64::try_from(price).map_err(|_| Error::<T>::ArithmeticOverflow)?.max(1))
		}
	}
}
