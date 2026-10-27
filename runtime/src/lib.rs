#![cfg_attr(not(feature = "std"), no_std)]
// `construct_runtime!` expands recursively per pallet; the default 128 is not enough for the
// number of pallets this runtime composes.
#![recursion_limit = "512"]

#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));


#[cfg(feature = "runtime-benchmarks")]
mod benchmarks;
pub mod configs;
pub mod apis;
extern crate alloc;
use alloc::vec::Vec;
use sp_runtime::{
	generic, impl_opaque_keys,
	traits::{BlakeTwo256, IdentifyAccount, Verify},
	MultiAddress, MultiSignature,
};
#[cfg(feature = "std")]
use sp_version::NativeVersion;
use sp_version::RuntimeVersion;
use pallet_session::historical as pallet_session_historical;

pub use frame_system::Call as SystemCall;
pub use pallet_balances::Call as BalancesCall;
pub use pallet_timestamp::Call as TimestampCall;
#[cfg(any(feature = "std", test))]
pub use sp_runtime::BuildStorage;


/// Constant values used within the runtime.
pub mod constants;
use constants::{currency::*, time::*};
/// Generated voter bag information.
mod voter_bags;
pub mod genesis_config_presets;
use sp_core::{
	crypto::{ByteArray, KeyTypeId},
	 OpaqueMetadata, H160, H256, U256,
};
use fp_account::EthereumSignature;

use sp_runtime::{
	traits::{
		 Block as BlockT, DispatchInfoOf, Dispatchable, Get, 
		IdentityLookup, NumberFor, PostDispatchInfoOf, UniqueSaturatedInto
	},
	transaction_validity::{TransactionValidity,TransactionValidityError},
	ApplyExtrinsicResult, ConsensusEngineId, ExtrinsicInclusionMode, Permill,
};
// mod genesis;
/// Opaque types. These are used by the CLI to instantiate machinery that don't need to know
/// the specifics of the runtime. They can then be made to be agnostic over specific formats
/// of data like extrinsics, allowing for them to continue syncing the network through upgrades
/// to even the core data structures.
pub mod opaque {
	use super::*;
	use sp_runtime::{
		generic,
		traits::{BlakeTwo256, Hash as HashT},
	};

	pub use sp_runtime::OpaqueExtrinsic as UncheckedExtrinsic;

	/// Opaque block header type.
	pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
	/// Opaque block type.
	pub type Block = generic::Block<Header, UncheckedExtrinsic>;
	/// Opaque block identifier type.
	pub type BlockId = generic::BlockId<Block>;
	/// Opaque block hash type.
	pub type Hash = <BlakeTwo256 as HashT>::Output;
}

// impl_opaque_keys! {
// 	pub struct SessionKeys {
// 		pub aura: Aura,
// 		pub grandpa: Grandpa,
// 	}
// }

// To learn more about runtime versioning, see:
// https://docs.substrate.io/main-docs/build/upgrade#runtime-versioning
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
	spec_name: alloc::borrow::Cow::Borrowed("sunchain-runtime"),
	impl_name: alloc::borrow::Cow::Borrowed("sunchain-runtime"),
	authoring_version: 1,
	// The version of the runtime specification. A full node will not attempt to use its native
	//   runtime in substitute for the on-chain Wasm runtime unless all of `spec_name`,
	//   `spec_version`, and `authoring_version` are the same between Wasm and native.
	// This value is set to 100 to notify Polkadot-JS App (https://polkadot.js.org/apps) to use
	//   the compatible custom types.
	spec_version: 108,
	impl_version: 6,
	apis: apis::RUNTIME_API_VERSIONS,
	transaction_version: 8,
	system_version: 2,
};

/// The BABE epoch configuration at genesis.
pub const BABE_GENESIS_EPOCH_CONFIG: sp_consensus_babe::BabeEpochConfiguration =
	sp_consensus_babe::BabeEpochConfiguration {
		c: PRIMARY_PROBABILITY,
		allowed_slots: sp_consensus_babe::AllowedSlots::PrimaryAndSecondaryPlainSlots,
	};

mod block_times {
	/// This determines the average expected block time that we are targeting. Blocks will be
	/// produced at a minimum duration defined by `SLOT_DURATION`. `SLOT_DURATION` is picked up by
	/// `pallet_timestamp` which is in turn picked up by `pallet_aura` to implement `fn
	/// slot_duration()`.
	///
	/// Change this to adjust the block time.
	pub const MILLI_SECS_PER_BLOCK: u64 = 3000;

	// NOTE: Currently it is not possible to change the slot duration after the chain has started.
	// Attempting to do so will brick block production.
	pub const SLOT_DURATION: u64 = MILLI_SECS_PER_BLOCK;
}
pub use block_times::*;

// Time is measured by number of blocks.
pub const MINUTES: BlockNumber = 60_000 / (MILLI_SECS_PER_BLOCK as BlockNumber);
pub const HOURS: BlockNumber = MINUTES * 60;
pub const DAYS: BlockNumber = HOURS * 24;

pub const BLOCK_HASH_COUNT: BlockNumber = 2400;

// Unit = the base number of indivisible units for balances
pub const UNIT: Balance = 1_000_000_000_000_000_000;
pub const MILLI_UNIT: Balance = 1_000_000_000;
pub const MICRO_UNIT: Balance = 1_000_000;

/// Existential deposit.
pub const EXISTENTIAL_DEPOSIT: Balance = MILLI_UNIT;

/// The version information used to identify this runtime when compiled natively.
#[cfg(feature = "std")]
pub fn native_version() -> NativeVersion {
	NativeVersion { runtime_version: VERSION, can_author_with: Default::default() }
}

/// Alias to 512-bit hash when used in the context of a transaction signature on the chain.
//pub type Signature = MultiSignature;
pub type Signature = EthereumSignature;

/// Some way of identifying an account on the chain. We intentionally make it equivalent
/// to the public key of our transaction signing scheme.
pub type AccountId = <<Signature as Verify>::Signer as IdentifyAccount>::AccountId;

/// Balance of an account.
pub type Balance = u128;

/// Type used for expressing timestamp.
pub type Moment = u64;

/// Index of a transaction in the chain.
pub type Nonce = u32;

/// A hash of some data used by the chain.
pub type Hash = sp_core::H256;

/// An index to a block.
pub type BlockNumber = u32;

/// The address format for describing accounts.
//pub type Address = MultiAddress<AccountId, ()>;
/// The address format for describing accounts.
pub type Address = AccountId;

/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;

/// Block type as expected by this runtime.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;

/// A Block signed with a Justification
pub type SignedBlock = generic::SignedBlock<Block>;

/// BlockId type as expected by this runtime.
pub type BlockId = generic::BlockId<Block>;

/// The `TransactionExtension` to the basic transaction logic.
// pub type TxExtension = (
// 	frame_system::CheckNonZeroSender<Runtime>,
// 	frame_system::CheckSpecVersion<Runtime>,
// 	frame_system::CheckTxVersion<Runtime>,
// 	frame_system::CheckGenesis<Runtime>,
// 	frame_system::CheckEra<Runtime>,
// 	frame_system::CheckNonce<Runtime>,
// 	frame_system::CheckWeight<Runtime>,
// 	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
// 	frame_metadata_hash_extension::CheckMetadataHash<Runtime>,
// 	frame_system::WeightReclaim<Runtime>,
// );

/// Unchecked extrinsic type as expected by this runtime.
// pub type UncheckedExtrinsic =
// 	generic::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;

/// The SignedExtension to the basic transaction logic.
// pub type SignedExtra = cumulus_pallet_weight_reclaim::StorageWeightReclaim<
// 	Runtime,
// 	(
// 		frame_system::CheckNonZeroSender<Runtime>,
// 		frame_system::CheckSpecVersion<Runtime>,
// 		frame_system::CheckTxVersion<Runtime>,
// 		frame_system::CheckGenesis<Runtime>,
// 		frame_system::CheckEra<Runtime>,
// 		frame_system::CheckNonce<Runtime>,
// 		frame_system::CheckWeight<Runtime>,
// 		pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
// 		frame_metadata_hash_extension::CheckMetadataHash<Runtime>,
// 		frame_system::WeightReclaim<Runtime>,
// 	),
// >;

pub type TxExtension = (
	frame_system::CheckNonZeroSender<Runtime>,
	frame_system::CheckSpecVersion<Runtime>,
	frame_system::CheckTxVersion<Runtime>,
	frame_system::CheckGenesis<Runtime>,
	frame_system::CheckEra<Runtime>,
	frame_system::CheckNonce<Runtime>,
	frame_system::CheckWeight<Runtime>,
	pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
	frame_metadata_hash_extension::CheckMetadataHash<Runtime>,
	frame_system::WeightReclaim<Runtime>,
);

/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic =
	fp_self_contained::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;

/// Extrinsic type that has already been checked.
pub type CheckedExtrinsic =
	fp_self_contained::CheckedExtrinsic<AccountId, RuntimeCall, TxExtension, H160>;

/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<RuntimeCall, TxExtension>;

impl fp_self_contained::SelfContainedCall for RuntimeCall {
	type SignedInfo = H160;

	fn is_self_contained(&self) -> bool {
		match self {
			RuntimeCall::Ethereum(call) => call.is_self_contained(),
			_ => false,
		}
	}

	fn check_self_contained(&self) -> Option<Result<Self::SignedInfo, TransactionValidityError>> {
		match self {
			RuntimeCall::Ethereum(call) => call.check_self_contained(),
			_ => None,
		}
	}

	fn validate_self_contained(
		&self,
		info: &Self::SignedInfo,
		dispatch_info: &DispatchInfoOf<RuntimeCall>,
		len: usize,
	) -> Option<TransactionValidity> {
		match self {
			RuntimeCall::Ethereum(call) => call.validate_self_contained(info, dispatch_info, len),
			_ => None,
		}
	}

	fn pre_dispatch_self_contained(
		&self,
		info: &Self::SignedInfo,
		dispatch_info: &DispatchInfoOf<RuntimeCall>,
		len: usize,
	) -> Option<Result<(), TransactionValidityError>> {
		match self {
			RuntimeCall::Ethereum(call) => {
				call.pre_dispatch_self_contained(info, dispatch_info, len)
			}
			_ => None,
		}
	}

	fn apply_self_contained(
		self,
		info: Self::SignedInfo,
	) -> Option<sp_runtime::DispatchResultWithInfo<PostDispatchInfoOf<Self>>> {
		match self {
			call @ RuntimeCall::Ethereum(pallet_ethereum::Call::transact { .. }) => {
				Some(call.dispatch(RuntimeOrigin::from(
					pallet_ethereum::RawOrigin::EthereumTransaction(info),
				)))
			}
			_ => None,
		}
	}
}

/// All migrations of the runtime, aside from the ones declared in the pallets.
///
/// This can be a tuple of types, each implementing `OnRuntimeUpgrade`.
#[allow(unused_parens)]
type Migrations = (
	// P2-B: `Position` gained `funding_index`, which changes its SCALE encoding. Gated on the
	// on-chain storage version (Phase 1 declared none, so it is `0`).
	frame_support::migrations::VersionedMigration<
		0,
		1,
		pallet_exchange_core::migrations::v1::Migration<Runtime>,
		pallet_exchange_core::Pallet<Runtime>,
		<Runtime as frame_system::Config>::DbWeight,
	>,
	// P2-C §9.1: adds the `SubaccountId` dimension to every per-account key.
	frame_support::migrations::VersionedMigration<
		1,
		2,
		pallet_exchange_core::migrations::v2::Migration<Runtime>,
		pallet_exchange_core::Pallet<Runtime>,
		<Runtime as frame_system::Config>::DbWeight,
	>,
	// Drops the Root-set mark price cache that `pallet-oracle` replaced.
	frame_support::migrations::VersionedMigration<
		2,
		3,
		pallet_exchange_core::migrations::v3::Migration<Runtime>,
		pallet_exchange_core::Pallet<Runtime>,
		<Runtime as frame_system::Config>::DbWeight,
	>,
	// S3-1: `Order` gained a `subaccount` field; drop pre-S3-1 orders that no longer decode.
	frame_support::migrations::VersionedMigration<
		3,
		4,
		pallet_exchange_core::migrations::v4::Migration<Runtime>,
		pallet_exchange_core::Pallet<Runtime>,
		<Runtime as frame_system::Config>::DbWeight,
	>,
);

/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
	Runtime,
	Block,
	frame_system::ChainContext<Runtime>,
	Runtime,
	AllPalletsWithSystem,
	Migrations,
>;

mod precompiles;

// Create the runtime by composing the FRAME pallets that were previously configured.
#[frame_support::runtime]
mod runtime {
    
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeFreezeReason,
		RuntimeHoldReason,
		RuntimeSlashReason,
		RuntimeLockId,
		RuntimeTask,
		RuntimeViewFunction
	)]
	pub struct Runtime;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;

	#[runtime::pallet_index(1)]
	pub type Timestamp = pallet_timestamp;

	// #[runtime::pallet_index(2)]
	// pub type Aura = pallet_aura;

	#[runtime::pallet_index(3)]
	pub type Grandpa = pallet_grandpa;

	#[runtime::pallet_index(4)]
	pub type Balances = pallet_balances;

	#[runtime::pallet_index(5)]
	pub type TransactionPayment = pallet_transaction_payment;

	#[runtime::pallet_index(6)]
	pub type Sudo = pallet_sudo;

	// Include the custom logic from the pallet-template in the runtime.
	#[runtime::pallet_index(7)]
	pub type Template = pallet_template;

	#[runtime::pallet_index(8)]
	pub type Babe = pallet_babe; //::Pallet<Runtime>;
	// Authorship must be before session in order to note author in the correct session and era
	// for im-online and staking.
	#[runtime::pallet_index(9)]
	pub type Authorship = pallet_authorship; //::Pallet<Runtime>;

	#[runtime::pallet_index(10)]
	pub type Staking = pallet_staking; //::Pallet<Runtime>;

	#[runtime::pallet_index(11)]
	pub type Session = pallet_session; //::Pallet<Runtime>;

	#[runtime::pallet_index(12)]
	pub type ImOnline = pallet_im_online; //::Pallet<Runtime>;

	#[runtime::pallet_index(13)]
	pub type AuthorityDiscovery = pallet_authority_discovery; //::Pallet<Runtime>;

	#[runtime::pallet_index(14)]
	pub type Historical = pallet_session_historical; //::Pallet<Runtime>;

	#[runtime::pallet_index(15)]
	pub type VoterList = pallet_bags_list::Pallet<Runtime, Instance1>;

	#[runtime::pallet_index(16)]
	pub type Offences = pallet_offences::Pallet<Runtime>;

	#[runtime::pallet_index(17)]
	pub type Ethereum = pallet_ethereum; 

	#[runtime::pallet_index(18)]
	pub type EVM = pallet_evm;

	#[runtime::pallet_index(19)]
	pub type BaseFee = pallet_base_fee;

	#[runtime::pallet_index(20)]
	pub type EVMChainId = pallet_evm_chain_id;

	#[runtime::pallet_index(21)]
	pub type Assets = pallet_assets;

	// Custom orderbook DEX exchange core.
	#[runtime::pallet_index(22)]
	pub type ExchangeCore = pallet_exchange_core;

	// Insurance fund: the backstop spent before any loss is socialised (Phase 2 §8.4).
	#[runtime::pallet_index(23)]
	pub type Insurance = pallet_insurance;

	// Permissionless liquidation entry point (Phase 2 §8.3).
	#[runtime::pallet_index(24)]
	pub type Liquidation = pallet_liquidation;

// Multi-source price oracle: the index price funding and liquidations are measured against
// (Phase 2 P2-A). Registered here for the first time — until now the core ran on `NoOracle`.
#[runtime::pallet_index(25)]
pub type Oracle = pallet_oracle;

// SunLead deterministic-leader consensus (P2): computes & exposes the epoch
// authority snapshot and stake-weighted schedule. Does NOT change BABE/GRANDPA
// authoring yet — that is P3. Registered with the next free index (26).
#[runtime::pallet_index(26)]
pub type SunLead = pallet_sunlead;
}
