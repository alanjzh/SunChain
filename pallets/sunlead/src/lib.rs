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
	/// remains the block author (default). This flag is **genesis-only**: it is
	/// set by `GenesisConfig::sunlead_active` and there is no runtime dispatch to
	/// flip it, so the active authoring engine is fixed for the lifetime of the
	/// chain and cannot be toggled by governance mid-flight (which would desync
	/// node / Runtime consensus — see Remediation 4.1). The P5 activation state
	/// machine will replace this boolean with a finalized `activation_epoch`
	/// transition. Exposed to the node via `SunLeadApi::is_active` so the
	/// verifier/worker can gate on it.
	#[pallet::storage]
	pub type SunLeadActive<T> = StorageValue<_, bool, ValueQuery>;

	/// Genesis configuration for `pallet-sunlead`.
	///
	/// Lets a chain spec decide whether SunLead is the active authoring consensus at
	/// genesis (the node's startup gate reads `is_active` at block 0, so the flag must
	/// be set here). There is no runtime call to flip it later, so this is the only
	/// way to choose the active authoring engine for the chain — the P5 activation
	/// state machine will supersede this with a finalized epoch transition. The
	/// initial authority snapshot + schedule are also built here because
	/// `on_initialize` does not run on the genesis block, yet the first
	/// SunLead-authored block needs the snapshot to exist at the genesis state.
	#[derive(frame_support::DefaultNoBound)]
	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		/// Whether SunLead is the active authoring consensus at genesis.
		pub sunlead_active: bool,
		/// Schedule parameters frozen per epoch. Validated and persisted at genesis
		/// (they were previously never written, so this also fixes that bug). The
		/// value defaults to [`SunLeadScheduleParams::v1_defaults`]; make it
		/// configurable (and governance-controlled) once a `set_schedule_params`
		/// extrinsic exists. `#[serde(skip)]` keeps it out of the chain-spec JSON
		/// (no serde impl needed) while still letting `build()` persist it.
		#[serde(skip)]
		pub params: SunLeadScheduleParams,
		#[serde(skip)]
		pub _phantom: core::marker::PhantomData<T>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			// Validate and persist the schedule parameters. They are frozen into the
			// epoch snapshot at each boundary, but must be validated once here so a
			// misconfigured chain spec fails fast at genesis rather than producing an
			// un-authorable chain. (Previously `ScheduleParams` was never written, so
			// the `params` genesis field was silently ignored and the `Default` value
			// was always used — this now honors the configured value.)
			self.params
				.validate()
				.expect("invalid SunLead ScheduleParams in genesis config");
			ScheduleParams::<T>::put(self.params);

			SunLeadActive::<T>::put(self.sunlead_active);
			// Best-effort: build the epoch-0 snapshot + schedule so the very first
			// SunLead-authored block can derive its leader. Ignore failure (e.g. a
			// validator without a `sunl` key) so genesis never hard-fails the chain —
			// BABE still works when SunLead is inactive.
			let _ = Pallet::<T>::build_current_snapshot(0);
		}
	}

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
		/// Proposed / configured schedule parameters are outside the
		/// governance-acceptable range (see `SunLeadScheduleParams::validate`).
		InvalidScheduleParams,
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
			// Fail-fast: an active SunLead chain that still has no valid authority
			// snapshot after a rebuild attempt cannot author blocks. Refuse to run
			// rather than silently produce an un-authorable chain (e.g. validators
			// missing their `sunl` session key, or empty authorities / bad params).
			if SunLeadActive::<T>::get() && Snapshot::<T>::get().is_none() {
				panic!(
					"SunLead is active but no authority snapshot could be built; \
					 ensure every validator has a registered `sunl` session key and \
					 that the authority set / schedule params are valid"
				);
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
			// NOTE: this stored `epoch_index` / `activation_slot` are *session*-
			// snapshot bookkeeping (anchored to the session counter). On-chain
			// verification and the producer derive the schedule epoch directly from
			// the absolute, time-based slot (`slot / epoch_slots`); they do NOT use
			// these stored values. They are retained for the `ScheduleComputed`
			// event / `CurrentEpochIndex` and as the snapshot's own identity.
			let epoch: EpochIndex = session as u64;
			let authority_set_id: AuthoritySetId =
				pallet_grandpa::CurrentSetId::<T::Grandpa>::get();
			let params = ScheduleParams::<T>::get();
			params.validate().map_err(|_| Error::<T>::InvalidScheduleParams)?;

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
				params,
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
			let snapshot = Snapshot::<T>::get()?;
			let params = snapshot.params;
			// The epoch is anchored to the absolute slot clock (not the session
			// counter): `epoch = slot / epoch_slots`, `rel = slot % epoch_slots`.
			// Mirrors the verifier / producer so diagnostics agree with on-chain
			// verification.
			let epoch = slot / params.epoch_slots;
			let rel = slot % params.epoch_slots;
			let input = ScheduleInput {
				chain_id: snapshot.chain_id,
				authority_set_id: snapshot.authority_set_id,
				epoch_index: epoch,
				epoch_seed: snapshot.epoch_seed,
				authorities: snapshot
					.authorities
					.iter()
					.map(|a| (a.id.clone(), a.stake))
					.collect(),
				params,
			};
			sunlead_schedule::build_schedule(&input).ok().and_then(|s| s.leader_at(rel))
		}

		pub fn schedule_proof(slot: u64) -> Option<sunlead_schedule::SunLeadScheduleProof> {
			let snapshot = Snapshot::<T>::get()?;
			let params = snapshot.params;
			let epoch = slot / params.epoch_slots;
			let rel = slot % params.epoch_slots;
			let input = ScheduleInput {
				chain_id: snapshot.chain_id,
				authority_set_id: snapshot.authority_set_id,
				epoch_index: epoch,
				epoch_seed: snapshot.epoch_seed,
				authorities: snapshot
					.authorities
					.iter()
					.map(|a| (a.id.clone(), a.stake))
					.collect(),
				params,
			};
			sunlead_schedule::build_schedule(&input)
				.ok()
				.and_then(|s| s.schedule_proof(rel))
		}

		/// Whether SunLead is the currently active authoring consensus.
		pub fn is_active() -> bool {
			SunLeadActive::<T>::get()
		}
	}
}
