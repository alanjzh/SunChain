//! # pallet-fast-finality
//!
//! Runtime storage for the **Sunchain Fast Finality (SFF)** shadow protocol (F2).
//!
//! This pallet does **not** replace BABE/GRANDPA and does **not** change chain
//! state based on SFF certificates. During F2 it only:
//!
//! * stores each validator's BLS12-381 public key (G2, 96 bytes) together with
//!   a proof-of-possession, registered by the validator's staking account;
//! * exposes the raw key material so the `FastFinalityApi` runtime API can build
//!   a [`sff_primitives::FinalityAuthoritySet`] from the elected validators,
//!   their staking stake and their registered BLS keys.
//!
//! On-chain BLS verification is intentionally *not* performed here: BLS is a
//! std-only (ark) dependency and the runtime must remain `no_std` for the WASM
//! blob. The node verifies every proof-of-possession off-chain (with
//! `sff_crypto::pop_verify`) before trusting a key; the snapshot assembled by
//! the runtime API is therefore a *candidate* set that the node re-validates.
//!
//! A full `sffb` session-key type (with keystore-backed secret keys and
//! `OpaqueKeys` integration) is the production item for F3/F4; for F2 the
//! per-account registration below is sufficient for the shadow vote path.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::*;
use sff_primitives::{BlsPublicKey, BlsSignature};

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
	}

	/// One registered BLS key + proof-of-possession per staking account.
	///
	/// `None` until the account calls [`Pallet::register_bls_key`]. Until every
	/// elected validator has registered, the `FastFinalityApi` cannot assemble
	/// a complete authority set and SFF voting stays dormant.
	#[pallet::storage]
	pub type BlsKeys<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, (BlsPublicKey, BlsSignature), OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A BLS key + PoP was registered for `account`.
		BlsKeyRegistered {
			account: T::AccountId,
			public_key: BlsPublicKey,
		},
		/// An account over-wrote a previously registered BLS key.
		BlsKeyRotated {
			account: T::AccountId,
			public_key: BlsPublicKey,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The supplied public key is the all-zero (invalid) key.
		InvalidPublicKey,
		/// The supplied proof-of-possession is the all-zero (invalid) signature.
		InvalidPop,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Register (or rotate) the caller's BLS12-381 public key and a
		/// proof-of-possession.
		///
		/// The key is stored verbatim; the runtime does **not** verify the PoP
		/// (BLS is std-only). The node MUST verify `pop` against `public_key`
		/// off-chain before using the key in an SFF authority set.
		#[pallet::call_index(0)]
		#[pallet::weight(<T as frame_system::Config>::DbWeight::get().reads_writes(1, 1))]
		pub fn register_bls_key(
			origin: OriginFor<T>,
			public_key: BlsPublicKey,
			pop: BlsSignature,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(!public_key.is_zero(), Error::<T>::InvalidPublicKey);
			ensure!(!pop.is_zero(), Error::<T>::InvalidPop);

			if BlsKeys::<T>::contains_key(&who) {
				BlsKeys::<T>::insert(&who, (public_key, pop));
				Self::deposit_event(Event::BlsKeyRotated {
					account: who,
					public_key,
				});
			} else {
				BlsKeys::<T>::insert(&who, (public_key, pop));
				Self::deposit_event(Event::BlsKeyRegistered {
					account: who,
					public_key,
				});
			}
			Ok(())
		}
	}
}
