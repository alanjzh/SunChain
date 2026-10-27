//! # pallet-sunlead
//!
//! Runtime side of the **SunLead** deterministic-leader consensus (P2).
//!
//! This pallet does **not** replace BABE/GRANDPA authoring. During P2 it only:
//!
//! * snapshots the active authority set (account, `sunl` key, stake) at each
//!   session/epoch boundary,
//! * derives an epoch seed from finalized BABE randomness,
//! * computes the deterministic, stake-weighted schedule and its Merkle root via
//!   `sunlead-schedule`, and
//! * exposes all of the above through the `SunLeadApi` runtime API.
//!
//! The node (P3) consumes the snapshot + params to derive the schedule and to
//! verify each block's `sunl` digest before runtime execution.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::*;
use sp_consensus_slots::Slot;
use sp_core::H256;
use sp_runtime::traits::{Convert, Get, OpaqueKeys, SaturatedConversion};
use sp_std::vec::Vec;

use sunlead_primitives::{
	AuthoritySetId, EpochIndex, Stake, SunLeadAuthority, SunLeadAuthoritySnapshot, SunLeadId,
	SunLeadScheduleParams,
};
use sunlead_schedule::{build_schedule, ComputedSchedule, ScheduleInput};

/// Trait used to extract the `sunl` session key from the runtime's opaque
/// `SessionKeys` struct without the pallet knowing its concrete shape.
///
/// The runtime implements this for its `SessionKeys` type (see `runtime/src/configs/mod.rs`).
pub trait SunLeadKeyLookup {
	fn sunl_key(&self) -> Option<SunLeadId>;
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The session pallet. Its `Keys` type must equal [`Config::Keys`] and its
		/// validator id must be this runtime's `AccountId`.
		type Session: pallet_session::Config<
			Keys = Self::Keys,
			ValidatorId = <Self as frame_system::Config>::AccountId,
		>;
		/// The runtime's opaque session keys, carrying the `sunl` key.
		type Keys: OpaqueKeys + SunLeadKeyLookup + codec::FullCodec;
		/// Staking pallet, used to read per-validator exposure (stake).
		type Staking: pallet_staking::Config;
		/// Converts this runtime's `AccountId` into the staking pallet's account id
		/// (identity in practice; lets the pallet stay generic over both).
		type StakingAccountConvert: Convert<
			<Self as frame_system::Config>::AccountId,
			<Self::Staking as frame_system::Config>::AccountId,
		>;
		/// BABE pallet, used as the source of finalized epoch randomness.
		type Babe: pallet_babe::Config;
		/// GRANDPA pallet, used to read the current authority set id.
		type Grandpa: pallet_grandpa::Config;
		/// Chain id, bound into the schedule seed (cross-chain replay protection).
		type ChainId: Get<u64>;
	}

	/// Storage version for forward migrations.
	#[pallet::storage]
	pub type SunLeadStorageVersion<T> = StorageValue<_, u8, ValueQuery>;

	/// Active schedule parameters (frozen per epoch; default V1).
	#[pallet::storage]
	pub type ScheduleParams<T> = StorageValue<_, SunLeadScheduleParams, ValueQuery>;

	/// Index of the epoch whose schedule is currently stored.
	#[pallet::storage]
	pub type CurrentEpochIndex<T> = StorageValue<_, EpochIndex, ValueQuery>;

	/// Authority set id of the currently stored schedule.
	#[pallet::storage]
	pub type CurrentAuthoritySetId<T> = StorageValue<_, AuthoritySetId, ValueQuery>;

	/// Frozen, epoch-scoped authority snapshot.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type Snapshot<T> =
		StorageValue<_, SunLeadAuthoritySnapshot<<T as frame_system::Config>::AccountId>>;

	/// Fully derived epoch schedule (leaders / windows / Merkle root).
	#[pallet::storage]
	#[pallet::unbounded]
	pub type Schedule<T> = StorageValue<_, ComputedSchedule>;

	/// Last session index for which a schedule was built (rebuild on change).
	#[pallet::storage]
	pub type LastSession<T> = StorageValue<_, u32, ValueQuery>;

	/// Whether SunLead is the active authoring consensus. While `false`, BABE
	/// remains the block author (default). Flipped to `true` only at the
	/// governance-approved activation block (P5 migration). Exposed to the node
	/// via `SunLeadApi::is_active` so the verifier/worker can gate on it.
	#[pallet::storage]
	pub type SunLeadActive<T> = StorageValue<_, bool, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new epoch schedule was computed and stored.
		ScheduleComputed {
			epoch: EpochIndex,
			authority_set_id: AuthoritySetId,
			schedule_root: H256,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The deterministic schedule could not be built (e.g. infeasible input).
		ScheduleBuildFailed,
		/// A validator had no `sunl` session key registered.
		MissingSunLeadKey,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Enable/disable SunLead as the active authoring consensus.
		///
		/// Governance-gated: only `Root` may flip the switch. Until activated,
		/// BABE remains the block author (P2/P3 keep BABE running by default; the
		/// actual handoff to SunLead production is the P5 migration).
		#[pallet::call_index(0)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_sunlead_active(origin: OriginFor<T>, enabled: bool) -> DispatchResult {
			ensure_root(origin)?;
			SunLeadActive::<T>::put(enabled);
			Ok(())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			let session = pallet_session::CurrentIndex::<T::Session>::get();
			let needs_rebuild =
				Snapshot::<T>::get().is_none() || LastSession::<T>::get() != session;
			if needs_rebuild {
				if Self::build_current_snapshot(session).is_ok() {
					LastSession::<T>::put(session);
				}
			}
			// Approximate weight: a few storage reads plus the (cheap) rebuild reads.
			T::DbWeight::get().reads_writes(4, 3)
		}

		#[cfg(feature = "try-runtime")]
		fn on_runtime_upgrade() -> Weight {
			Self::set_storage_version();
			T::DbWeight::get().writes(1)
		}
	}

	impl<T: Config> Pallet<T> {
		/// Mark the current storage version.
		pub fn set_storage_version() {
			SunLeadStorageVersion::<T>::put(1u8);
		}

		/// Build (and store) the authority snapshot + schedule for `session`.
		fn build_current_snapshot(session: u32) -> DispatchResult {
			let epoch: EpochIndex = session as u64;
			let authority_set_id: AuthoritySetId =
				pallet_grandpa::CurrentSetId::<T::Grandpa>::get();
			let params = ScheduleParams::<T>::get();

			// Epoch seed from finalized BABE randomness.
			let seed_bytes = pallet_babe::Randomness::<T::Babe>::get();
			let epoch_seed = H256::from(seed_bytes);

			// Active validators + their `sunl` keys + stakes.
			let validators = pallet_session::Validators::<T::Session>::get();
			let era = pallet_staking::ActiveEra::<T::Staking>::get().map(|a| a.index).unwrap_or(0);

			let mut authorities: Vec<SunLeadAuthority<T::AccountId>> =
				Vec::with_capacity(validators.len());
			for account in validators.iter() {
				let keys = pallet_session::NextKeys::<T::Session>::get(account)
					.ok_or(Error::<T>::MissingSunLeadKey)?;
				let id = keys.sunl_key().ok_or(Error::<T>::MissingSunLeadKey)?;
				let staking_account = T::StakingAccountConvert::convert(account.clone());
				let exposure = pallet_staking::Pallet::<T::Staking>::eras_stakers(era, &staking_account);
				let stake: Stake = exposure.total.saturated_into::<u128>();
				authorities.push(SunLeadAuthority {
					account: account.clone(),
					id,
					stake,
				});
			}

			let authorities = sunlead_primitives::canonical_authorities(authorities);
			let activation_slot = Slot::from(epoch * params.epoch_slots);

			let input = ScheduleInput {
				chain_id: T::ChainId::get(),
				authority_set_id,
				epoch_index: epoch,
				epoch_seed,
				authorities: authorities.iter().map(|a| (a.id.clone(), a.stake)).collect(),
				params,
			};
			let sched =
				build_schedule(&input).map_err(|_| Error::<T>::ScheduleBuildFailed)?;

			let snapshot = SunLeadAuthoritySnapshot {
				chain_id: T::ChainId::get(),
				protocol_version: sunlead_primitives::SUNLEAD_PROTOCOL_VERSION,
				authority_set_id,
				epoch_index: epoch,
				activation_slot,
				authorities,
				epoch_seed,
				schedule_root: sched.root,
			};

			Snapshot::<T>::put(&snapshot);
			Schedule::<T>::put(&sched);
			CurrentEpochIndex::<T>::put(epoch);
			CurrentAuthoritySetId::<T>::put(authority_set_id);
			SunLeadStorageVersion::<T>::put(1u8);
			Self::deposit_event(Event::ScheduleComputed {
				epoch,
				authority_set_id,
				schedule_root: sched.root,
			});
			Ok(())
		}
	}

	/// Public accessors consumed by the `SunLeadApi` runtime API.
	impl<T: Config> Pallet<T> {
		pub fn configuration() -> SunLeadScheduleParams {
			ScheduleParams::<T>::get()
		}

		pub fn current_snapshot()
			-> Option<SunLeadAuthoritySnapshot<<T as frame_system::Config>::AccountId>>
		{
			Snapshot::<T>::get()
		}

		pub fn leader_at(slot: u64) -> Option<(u32, sunlead_primitives::WindowId)> {
			Schedule::<T>::get().and_then(|s| s.leader_at(slot))
		}

		pub fn schedule_proof(slot: u64) -> Option<sunlead_schedule::SunLeadScheduleProof> {
			Schedule::<T>::get().and_then(|s| s.schedule_proof(slot))
		}

		/// Whether SunLead is the currently active authoring consensus.
		pub fn is_active() -> bool {
			SunLeadActive::<T>::get()
		}
	}
}
