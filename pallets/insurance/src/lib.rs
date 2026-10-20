//! Insurance fund: the backstop that absorbs losses an account can no longer pay.
//!
//! Phase 2 design §8.4. The fund is spent **before** any loss is socialised to profitable
//! traders (ADL); skipping the fund is a protocol violation, not a policy choice.
//!
//! The fund holds real collateral in its own pot account and settles in the same asset the
//! exchange core uses, so covering a shortfall is a plain transfer: it puts back exactly the
//! money custody paid out on the winner's behalf.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use exchange_primitives::{InsuranceFund, QuoteAmount};
	use frame_support::{
		pallet_prelude::*,
		traits::{
			tokens::{fungibles::{Inspect, Mutate}, Preservation},
			Get,
		},
		PalletId,
	};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::{AccountIdConversion, AtLeast32BitUnsigned};

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Fungible asset adapter for the collateral token.
		type Assets: Inspect<Self::AccountId, AssetId = Self::AssetId, Balance = Self::Balance>
			+ Mutate<Self::AccountId>;

		type AssetId: Parameter + Member + Copy + MaxEncodedLen + TypeInfo;

		type Balance: Parameter
			+ Member
			+ Copy
			+ MaxEncodedLen
			+ TypeInfo
			+ AtLeast32BitUnsigned
			+ TryFrom<QuoteAmount>
			+ TryInto<QuoteAmount>;

		/// Deterministic account that holds the fund.
		type PalletId: Get<PalletId>;

		/// The single collateral asset the fund is denominated in.
		type CollateralAssetId: Get<Self::AssetId>;

		/// Where the fund pays out when it covers a shortfall: the exchange core custody
		/// account, which is short by exactly that amount.
		type Beneficiary: Get<Self::AccountId>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Collateral was added to the fund.
		Deposited { who: T::AccountId, amount: QuoteAmount },
		/// The fund paid out to cover uncollectable losses.
		ShortfallCovered { amount: QuoteAmount },
		/// The fund was credited (liquidation penalties, fee share).
		Credited { amount: QuoteAmount },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The fund does not hold enough to cover the shortfall.
		FundExhausted,
		InvalidAmount,
		AssetTransferFailed,
		AmountConversionFailed,
	}

	/// Lifetime total paid out by the fund.
	#[pallet::storage]
	pub type TotalCovered<T> = StorageValue<_, QuoteAmount, ValueQuery>;

	/// Lifetime total credited to the fund.
	#[pallet::storage]
	pub type TotalCredited<T> = StorageValue<_, QuoteAmount, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Adds `amount` of collateral to the fund. Anyone may top it up.
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::from_parts(50_000_000, 0))]
		pub fn deposit(origin: OriginFor<T>, amount: QuoteAmount) -> DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(amount > 0, Error::<T>::InvalidAmount);
			let balance = Self::to_balance(amount)?;
			T::Assets::transfer(
				T::CollateralAssetId::get(),
				&who,
				&Self::pot_account(),
				balance,
				Preservation::Expendable,
			)
			.map_err(|_| Error::<T>::AssetTransferFailed)?;
			Self::deposit_event(Event::Deposited { who, amount });
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Deterministic account holding the fund's collateral.
		pub fn pot_account() -> T::AccountId {
			T::PalletId::get().into_account_truncating()
		}

		fn to_balance(amount: QuoteAmount) -> Result<T::Balance, DispatchError> {
			amount.try_into().map_err(|_| Error::<T>::AmountConversionFailed.into())
		}

		fn to_quote(balance: T::Balance) -> Result<QuoteAmount, DispatchError> {
			balance.try_into().map_err(|_| Error::<T>::AmountConversionFailed.into())
		}
	}

	impl<T: Config> InsuranceFund<QuoteAmount> for Pallet<T> {
		fn cover(shortfall: QuoteAmount) -> DispatchResult {
			ensure!(shortfall > 0, Error::<T>::InvalidAmount);
			// All-or-nothing: the caller treats an error as "fund exhausted" and falls
			// through to ADL, so a partial payment would be silently mishandled.
			ensure!(Self::available() >= shortfall, Error::<T>::FundExhausted);

			let balance = Self::to_balance(shortfall)?;
			T::Assets::transfer(
				T::CollateralAssetId::get(),
				&Self::pot_account(),
				&T::Beneficiary::get(),
				balance,
				Preservation::Expendable,
			)
			.map_err(|_| Error::<T>::AssetTransferFailed)?;

			TotalCovered::<T>::mutate(|total| *total = total.saturating_add(shortfall));
			Self::deposit_event(Event::ShortfallCovered { amount: shortfall });
			Ok(())
		}

		fn credit(surplus: QuoteAmount) -> DispatchResult {
			ensure!(surplus > 0, Error::<T>::InvalidAmount);
			let balance = Self::to_balance(surplus)?;
			T::Assets::transfer(
				T::CollateralAssetId::get(),
				&T::Beneficiary::get(),
				&Self::pot_account(),
				balance,
				Preservation::Expendable,
			)
			.map_err(|_| Error::<T>::AssetTransferFailed)?;

			TotalCredited::<T>::mutate(|total| *total = total.saturating_add(surplus));
			Self::deposit_event(Event::Credited { amount: surplus });
			Ok(())
		}

		fn available() -> QuoteAmount {
			Self::to_quote(T::Assets::balance(
				T::CollateralAssetId::get(),
				&Self::pot_account(),
			))
			.unwrap_or_default()
		}
	}
}
