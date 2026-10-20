use frame_support::{
	derive_impl, parameter_types, PalletId,
	traits::{ConstBool, ConstI64, ConstU128, ConstU32, ConstU64, ConstU8,
			VariantCountOf, EitherOfDiverse, Nothing, AsEnsureOriginWithArg,
			tokens::imbalance::ResolveTo,FindAuthor, OnFinalize, OnTimestampSet
	},
	weights::{
		constants::{RocksDbWeight, WEIGHT_REF_TIME_PER_SECOND},
		IdentityFee, Weight, NoFee
	},
};

use sp_core::{
	crypto::{ByteArray, KeyTypeId},
	 OpaqueMetadata, H160, H256, U256,
};
use frame_system::{
	limits::{BlockLength, BlockWeights}, 
	offchain::{CreateInherent, CreateTransactionBase}, 
	EnsureRoot, EnsureSigned};
use frame_election_provider_support::{
	bounds::{ElectionBounds, ElectionBoundsBuilder},
	onchain, BalancingConfig, ElectionDataProvider, SequentialPhragmen, VoteWeight,
};	
use pallet_transaction_payment::{ConstFeeMultiplier, FungibleAdapter, Multiplier};
use sp_consensus_aura::sr25519::AuthorityId as AuraId;
use sp_runtime::{curve::PiecewiseLinear, traits::{ConvertInto, Identity, OpaqueKeys, Zero}, traits::One, transaction_validity::{TransactionPriority, TransactionSource, TransactionValidity}, offchain::Timestamp, Perbill, Vec, OpaqueExtrinsic};
use sp_runtime::{
	generic, impl_opaque_keys,
	traits::{
		BlakeTwo256, Block as BlockT, DispatchInfoOf, Dispatchable, Get, IdentifyAccount,
		IdentityLookup, NumberFor, PostDispatchInfoOf, UniqueSaturatedInto, Verify,
	},
	transaction_validity::{ TransactionValidityError},
	ApplyExtrinsicResult, ConsensusEngineId, ExtrinsicInclusionMode, Permill,
};
use sp_version::RuntimeVersion;
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;

use pallet_im_online::Call as ImOnlineCall;
use crate::{precompiles::FrontierPrecompiles, BaseFee, EVMChainId, UncheckedExtrinsic};
use core::marker::PhantomData;
use fp_account::{EthereumSignature, EthereumSigner};
use fp_evm::weight_per_gas;
use fp_rpc::TransactionStatus;
use codec::{Decode, Encode};
use exchange_primitives::NoOracle;
// Local module imports
use super::{
	AccountId, Assets, Balance, Balances, Block, BlockNumber, Hash, Nonce, PalletInfo, Runtime, // , Aura
	RuntimeCall, RuntimeEvent, RuntimeFreezeReason, RuntimeHoldReason, RuntimeOrigin, RuntimeTask,
	System, EXISTENTIAL_DEPOSIT, VERSION, MILLISECS_PER_BLOCK, VoterList, UNIT, MILLI_UNIT, DAYS,
	Session, Babe, EPOCH_DURATION_IN_SLOTS, Staking, ImOnline, constants::time::SLOT_DURATION,
	Grandpa, AuthorityDiscovery, voter_bags, Moment, Timestamp as TimestampPallet, Historical,
	Offences, 
};

use pallet_ethereum:: PostLogContent;
use pallet_evm::{
	Account as EVMAccount, EnsureAccountId20, FeeCalculator, IdentityAddressMapping, Runner,
};

const NORMAL_DISPATCH_RATIO: Perbill = Perbill::from_percent(75);

parameter_types! {
	pub const BlockHashCount: BlockNumber = 2400;
	pub const Version: RuntimeVersion = VERSION;

	/// We allow for 2 seconds of compute with a 6 second average block time.
	pub RuntimeBlockWeights: BlockWeights = BlockWeights::with_sensible_defaults(
		Weight::from_parts(2u64 * WEIGHT_REF_TIME_PER_SECOND, u64::MAX),
		NORMAL_DISPATCH_RATIO,
	);
	pub RuntimeBlockLength: BlockLength = BlockLength::max_with_normal_ratio(5 * 1024 * 1024, NORMAL_DISPATCH_RATIO);
	pub const SS58Prefix: u8 = 42;
}

/// The default types are being injected by [`derive_impl`](`frame_support::derive_impl`) from
/// [`SoloChainDefaultConfig`](`struct@frame_system::config_preludes::SolochainDefaultConfig`),
/// but overridden as needed.
//#[derive_impl(frame_system::config_preludes::SolochainDefaultConfig)]
#[derive_impl(frame_system::config_preludes::SolochainDefaultConfig as frame_system::DefaultConfig)]
impl frame_system::Config for Runtime {
	/// The block type for the runtime.
	type Block = Block;
	/// Block & extrinsics weights: base values and limits.
	type BlockWeights = RuntimeBlockWeights;
	/// The maximum length of a block (in bytes).
	type BlockLength = RuntimeBlockLength;
	/// The identifier used to distinguish between accounts.
	type AccountId = AccountId;
	/// The lookup mechanism to get account ID from whatever is passed in dispatchers.
	type Lookup = IdentityLookup<AccountId>;
	/// The type for storing how many extrinsics an account has signed.
	type Nonce = Nonce;
	/// The type for hashing blocks and tries.
	type Hash = Hash;
	/// Maximum number of block number to block hash mappings to keep (oldest pruned first).
	type BlockHashCount = BlockHashCount;
	/// The weight of database operations that the runtime can invoke.
	type DbWeight = RocksDbWeight;
	/// Version of the runtime.
	type Version = Version;
	/// The data to be stored in an account.
	type AccountData = pallet_balances::AccountData<Balance>;
	/// This is used as an identifier of the chain. 42 is the generic substrate prefix.
	type SS58Prefix = SS58Prefix;
	type MaxConsumers = frame_support::traits::ConstU32<16>;
}

// impl pallet_aura::Config for Runtime {
// 	type AuthorityId = AuraId;
// 	type DisabledValidators = ();
// 	type MaxAuthorities = ConstU32<32>;
// 	type AllowMultipleBlocksPerSlot = ConstBool<false>;
// 	type SlotDuration = pallet_aura::MinimumPeriodTimesTwo<Runtime>;
// }

// impl pallet_grandpa::Config for Runtime {
// 	type RuntimeEvent = RuntimeEvent;

// 	type WeightInfo = ();
// 	type MaxAuthorities = ConstU32<32>;
// 	type MaxNominators = ConstU32<0>;
// 	type MaxSetIdSessionEntries = ConstU64<0>;

// 	type KeyOwnerProof = sp_core::Void;
// 	type EquivocationReportSystem = ();
// }

impl pallet_timestamp::Config for Runtime {
	/// A timestamp: milliseconds since the unix epoch.
	type Moment = u64;
	// type OnTimestampSet = Aura;
	type OnTimestampSet = Babe;
	type MinimumPeriod = ConstU64<{ SLOT_DURATION / 2 }>;
	type WeightInfo = ();
}

impl pallet_balances::Config for Runtime {
	type MaxLocks = ConstU32<50>;
	type MaxReserves = ();
	type ReserveIdentifier = [u8; 8];
	/// The type for recording an account's balance.
	type Balance = Balance;
	/// The ubiquitous event type.
	type RuntimeEvent = RuntimeEvent;
	type DustRemoval = ();
	type ExistentialDeposit = ConstU128<EXISTENTIAL_DEPOSIT>;
	type AccountStore = System;
	type WeightInfo = pallet_balances::weights::SubstrateWeight<Runtime>;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = VariantCountOf<RuntimeFreezeReason>;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type DoneSlashHandler = ();
}

parameter_types! {
	pub FeeMultiplier: Multiplier = Multiplier::one();
}

impl pallet_transaction_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = FungibleAdapter<Balances, ()>;
	// type OnChargeTransaction = ();
	type OperationalFeeMultiplier = ConstU8<5>;
	type WeightToFee = IdentityFee<Balance>;
	type LengthToFee = IdentityFee<Balance>;
	// type WeightToFee = NoFee<Balance>;
	// type LengthToFee = NoFee<Balance>;
	type FeeMultiplierUpdate = ConstFeeMultiplier<FeeMultiplier>;
	type WeightInfo = pallet_transaction_payment::weights::SubstrateWeight<Runtime>;
}

impl pallet_sudo::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type WeightInfo = pallet_sudo::weights::SubstrateWeight<Runtime>;
}

/// Configure the pallet-template in pallets/template.
impl pallet_template::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = pallet_template::weights::SubstrateWeight<Runtime>;
}

// --------------------------------------------------------------------- pallet-assets
/// Phase 1 test USDC: the only collateral asset accepted by `pallet-exchange-core`.
///
/// These are **testnet** values and must stay in sync with the genesis of every chain spec
/// (`node/src/chain_spec.rs`). No bridged or real USDC exists in Phase 1; see the Phase 1
/// design §2.1 / §4.3 / §16.
pub const COLLATERAL_ASSET_ID: u32 = 1;
pub const COLLATERAL_ASSET_DECIMALS: u8 = 6;
/// One atomic unit, so closing a position can never leave un-removable dust behind.
pub const COLLATERAL_ASSET_MIN_BALANCE: u128 = 1;

parameter_types! {
	pub const AssetDeposit: Balance = UNIT; // single unit deposit to create an asset class
	pub const AssetAccountDeposit: Balance = UNIT;
	pub const ApprovalDeposit: Balance = 100 * MILLI_UNIT;
	pub const AssetsStringLimit: u32 = 50;
	pub const MetadataDepositBase: Balance = UNIT;
	pub const MetadataDepositPerByte: Balance = 10 * MILLI_UNIT;
	pub const ExchangePalletId: PalletId = PalletId(*b"exchcore");
	pub const InsurancePalletId: PalletId = PalletId(*b"insfund0");
}

impl pallet_assets::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Balance = u128;
	type AssetId = u32;
	type AssetIdParameter = codec::Compact<u32>;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type AssetDeposit = AssetDeposit;
	type MetadataDepositBase = MetadataDepositBase;
	type MetadataDepositPerByte = MetadataDepositPerByte;
	type ApprovalDeposit = ApprovalDeposit;
	type StringLimit = AssetsStringLimit;
	type Holder = ();
	type Freezer = ();
	type Extra = ();
	type WeightInfo = pallet_assets::weights::SubstrateWeight<Runtime>;
	type CallbackHandle = ();
	type AssetAccountDeposit = AssetAccountDeposit;
	type RemoveItemsLimit = ConstU32<1000>;
	// TEMPORARY: only exists when `pallet-assets/runtime-benchmarks` is enabled, which is
	// currently disabled (see the note in `runtime/Cargo.toml`). Restore together with it.
	// #[cfg(feature = "runtime-benchmarks")]
	// type BenchmarkHelper = ();
}

// --------------------------------------------------------------------- pallet-exchange-core
parameter_types! {
	/// Per-block budget reserved for `pallet-exchange-core` user actions (design §11.2).
	///
	/// 50% of the maximum block reference time (`RuntimeBlockWeights` allows 2s) and 3 MiB of
	/// PoV. Without this cap the generic transaction pool could fill a whole block with order
	/// flow and starve staking / governance / EVM traffic.
	///
	/// The budget is charged with the **actual** (post-refund) weight of each action, not the
	/// declared one, so a cheap resting order does not consume the whole budget. Measured on
	/// 2026-09-14 (`--steps 50 --repeat 20`, AMD Ryzen 7 PRO 4750U):
	///
	/// | action | ref_time | PoV |
	/// |---|---:|---:|
	/// | `place_order` resting (0 matches / 0 scan) | ~1.4 ms | ~6 KiB |
	/// | per executed maker match | ~65 µs | ~2.6 KiB |
	/// | per stale maker removed | ~50 µs | ~2.6 KiB |
	/// | per price level scanned | ~8 µs | ~2.5 KiB |
	///
	/// The 3 MiB PoV side is sized so that even one worst-case action (1024 level scans,
	/// ~2.9 MiB) still fits; only a *second* such action in the same block is rejected.
	/// Re-measure on the target hardware before any testnet carries value.
	pub ExchangeOperationalWeightBudget: Weight =
		Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 3 * 1024 * 1024);
	/// Coarse backstop on how many exchange actions a single block accepts.
	pub const MaxExchangeActionsPerBlock: u32 = 256;
}

parameter_types! {
	/// Funding: sample every 60 blocks (~3 min), tick every 1_200 blocks (~1 h).
	pub const FundingSamplePeriod: u32 = 60;
	pub const FundingTickPeriod: u32 = 1_200;
	/// A full funding rate is charged over 9_600 blocks (~8 h), so a 1 h tick charges 1/8.
	pub const FundingRatePeriod: u32 = 9_600;
	pub const MaxPremiumSamples: u32 = 64;
	pub const MaxPremiumVotes: u32 = 32;
	/// dYdX `RemovedTailSampleRatioPpm`: drop 5% from both ends before averaging.
	pub const RemovedTailSampleRatioPpm: u32 = 50_000;
	pub const DefaultFundingPpm: i32 = 0;
	/// dYdX `FundingRateClampFactorPpm`: |rate| <= 0.6 * (initial - maintenance margin).
	pub const FundingRateClampFactorPpm: u32 = 600_000;
	pub const MaxAbsPremiumPpm: u32 = 60_000;
	/// 500 USDC (6 decimals) of impact notional, as in dYdX.
	pub const ImpactNotionalQuote: u128 = 500_000_000;
}

/// secp256k1 public key whose account id is this chain's 20-byte EVM address.
///
/// `EthereumSigner` itself does not implement `DecodeWithMemTracking`, which this SDK
/// version requires of every `BoundedVec` element — hence the newtype.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	scale_info::TypeInfo,
	codec::MaxEncodedLen,
)]
pub struct ExchangeSigner(pub [u8; 20]);

impl sp_runtime::traits::IdentifyAccount for ExchangeSigner {
	type AccountId = AccountId;
	fn into_account(self) -> AccountId {
		AccountId::from(sp_core::H160(self.0))
	}
}

/// secp256k1 signature over `keccak256(payload)`, recoverable to an EVM address.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	scale_info::TypeInfo,
	codec::MaxEncodedLen,
)]
pub struct ExchangeSignature(pub [u8; 65]);

impl sp_runtime::traits::Verify for ExchangeSignature {
	type Signer = ExchangeSigner;
	fn verify<L: sp_runtime::traits::Lazy<[u8]>>(&self, mut msg: L, signer: &AccountId) -> bool {
		// keccak, like the rest of the EVM account model — not the Substrate default blake2.
		let hash = sp_io::hashing::keccak_256(msg.get());
		// Ethereum address = keccak256(uncompressed pubkey)[12..].
		let Ok(uncompressed) = sp_io::crypto::secp256k1_ecdsa_recover(&self.0, &hash) else {
			return false
		};
		let digest = sp_io::hashing::keccak_256(&uncompressed);
		let mut address = [0u8; 20];
		address.copy_from_slice(&digest[12..]);
		AccountId::from(sp_core::H160(address)) == *signer
	}
}

impl pallet_oracle::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	/// Whitelisting a price source is a governance action, not something a source can do for
	/// itself.
	type ForceOrigin = EnsureRoot<AccountId>;
	/// dYdX `MinExchanges`: a single reporter cannot produce a price.
	type MinSources = MinSources;
	type MaxSources = MaxSources;
	type MaxMarkets = MaxMarkets;
	type MaxPriceAge = MaxPriceAge;
	type MinPriceChangePpm = MinPriceChangePpm;
	// No `weights.rs` has been generated for this pallet yet; `Pallet` computes
	// trivial weights. Bounded work either way.
	type WeightInfo = pallet_oracle::weights::WeightInfo<Runtime>;
	/// The feed signs inside the call; the real scheme, not a stand-in.
	type Signature = EthereumSignature;
	type Signer = EthereumSigner;
}

parameter_types! {
	/// How many *fresh* sources must agree before an index price exists.
	pub const MinSources: u32 = 2;
	pub const MaxSources: u32 = 8;
	pub const MaxMarkets: u32 = 64;
	/// A quote older than 60 blocks (~2 min at 2s blocks) no longer counts.
	pub const MaxPriceAge: BlockNumber = 60;
	/// dYdX `MinPriceChangePpm`: ignore moves below 0.05%, so noise does not rewrite state.
	pub const MinPriceChangePpm: u32 = 500;
}

impl pallet_exchange_core::Config for Runtime {
	type Signature = ExchangeSignature;
	type Signer = ExchangeSigner;
	type RuntimeEvent = RuntimeEvent;
	type Assets = Assets;
	type AssetId = u32;
	type Balance = u128;
	type ForceOrigin = EnsureRoot<AccountId>;
	type PalletId = ExchangePalletId;
	/// The single collateral asset accepted by the core: the Phase 1 test USDC created at
	/// genesis. Real / bridged USDC is explicitly out of scope for Phase 1 (design §2.1, §16).
	type CollateralAssetId = ConstU32<COLLATERAL_ASSET_ID>;
	// Phase 1 pre-benchmark conservative values, taken from the design §4.2 table. They are
	// deliberately NOT production values: they must be re-derived from the benchmark output
	// (block weight, PoV and the target hardware) before any testnet carries value.
	type MaxMarkets = ConstU32<64>;
	type MaxSubaccountsPerAccount = ConstU32<16>;
	type MaxClearingMarkets = ConstU32<8>;
	type MaxOpenOrdersPerAccount = ConstU32<64>;
	type MaxPriceLevelsPerSide = ConstU32<1_024>;
	type MaxOrdersPerLevel = ConstU32<256>;
	type MaxMatchesPerAction = ConstU32<64>;
	type MaxActionsPerBatch = ConstU32<16>;
	type MaxPrunePerCall = ConstU32<64>;
	/// 28,800 blocks; with the current 3s block time this is ~24h.
	type MaxOrderLifetime = ConstU32<28_800>;
	/// 20 blocks; with the current 3s block time this is ~60s (design §4.2).
	// Liquidation (§8.2 / §8.3). Uncalibrated placeholders, like the rest of §4.2: they must be
	// re-derived from benchmarks before any testnet carries value.
	type MaintenanceBufferPpm = ConstU32<10_000>;
	type LiquidationPenaltyPpm = ConstU32<5_000>;
	type LiquidationKeeperRebatePpm = ConstU32<0>;
	type MaxLiquidationSlippagePpm = ConstU32<50_000>;
	type MinLiquidationLots = ConstU64<1>;
	type FundingSamplePeriod = FundingSamplePeriod;
	type FundingTickPeriod = FundingTickPeriod;
	type FundingRatePeriod = FundingRatePeriod;
	type MaxPremiumSamples = MaxPremiumSamples;
	type MaxPremiumVotes = MaxPremiumVotes;
	type RemovedTailSampleRatioPpm = RemovedTailSampleRatioPpm;
	type DefaultFundingPpm = DefaultFundingPpm;
	type FundingRateClampFactorPpm = FundingRateClampFactorPpm;
	type MaxAbsPremiumPpm = MaxAbsPremiumPpm;
	type ImpactNotionalQuote = ImpactNotionalQuote;
	type MaxExchangeActionsPerBlock = MaxExchangeActionsPerBlock;
	type ExchangeBlockWeightBudget = ExchangeOperationalWeightBudget;
	/// The multi-source oracle. Until now this was `NoOracle`, which meant the whole index
	/// price path — funding premium, liquidation pricing — was inert.
	// While benchmarking, the core needs a price it can be *given*: the real oracle's trait is
	// read-only, so `pallet_exchange_core`'s benchmarks cannot seed it. Swap in a stub that
	// lives in externalities. The oracle itself is measured separately by its own benchmarks.
	#[cfg(feature = "runtime-benchmarks")]
	type PriceOracle = pallet_exchange_core::benchmarking::BenchmarkOracle;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type PriceOracle = pallet_oracle::Pallet<Runtime>;
	/// Insurance fund backstop (P2-B §8.4).
	type Insurance = pallet_insurance::Pallet<Runtime>;
	type WeightInfo = pallet_exchange_core::weights::SubstrateWeight<Runtime>;
}

/// The exchange core custody account: where the insurance fund pays out to.
///
/// Taken from the core itself rather than re-derived, so the two can never drift apart.
pub struct ExchangeCustody;

impl Get<AccountId> for ExchangeCustody {
	fn get() -> AccountId {
		pallet_exchange_core::Pallet::<Runtime>::custody_account()
	}
}

impl pallet_liquidation::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Engine = pallet_exchange_core::Pallet<Runtime>;
	type MaxLiquidationsPerCall = ConstU32<8>;
	type MinAdlRoePpm = ConstI64<50_000>;
	type AdlPriceBandPpm = ConstU32<1_000>;
	type WeightInfo = pallet_liquidation::weights::SubstrateWeight<Runtime>;
}

impl pallet_insurance::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Assets = Assets;
	type AssetId = u32;
	type Balance = u128;
	type PalletId = InsurancePalletId;
	type CollateralAssetId = ConstU32<COLLATERAL_ASSET_ID>;
	type Beneficiary = ExchangeCustody;
}


/// Sunchain v1.6 enable staking
/// This is the configuration for the staking pallet.
parameter_types! {
	// NOTE: Currently it is not possible to change the epoch duration after the chain has started.
	//       Attempting to do so will brick block production.
	pub const EpochDuration: u64 = EPOCH_DURATION_IN_SLOTS;
	pub const ExpectedBlockTime: Moment = MILLISECS_PER_BLOCK;
	pub const ReportLongevity: u64 =
		BondingDuration::get() as u64 * SessionsPerEra::get() as u64 * EpochDuration::get();
}

impl CreateTransactionBase<pallet_babe::Call<Runtime>> for Runtime {
	type Extrinsic = sp_runtime::OpaqueExtrinsic;

	type RuntimeCall = RuntimeCall;
}

// impl CreateBare<pallet_babe::Call<Runtime>> for Runtime
// {
// 	fn create_bare(call: RuntimeCall) -> UncheckedExtrinsic {
// 		generic::UncheckedExtrinsic::new_bare(call).into()
// 	}
// }

impl CreateInherent<pallet_babe::Call<Runtime>> for Runtime {

	fn create_inherent(_call: Self::RuntimeCall) -> Self::Extrinsic {
		unimplemented!("offchain worker is not supported")
	}

	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		unimplemented!("offchain worker is not supported")
	}

	// fn create_bare(call: RuntimeCall) -> UncheckedExtrinsic {
	// 	UncheckedExtrinsic::new_bare(call)
	// }

	// fn create_bare(
	// 	call: <Self as CreateTransactionBase<pallet_babe::Call<Runtime>>>::RuntimeCall,
	// ) -> <Self as CreateTransactionBase<pallet_babe::Call<Runtime>>>::Extrinsic {
	// 	// 根据你的 runtime 定义返回 Extrinsic
	// 	// 占位：可直接用 Self::UncheckedExtrinsic::new_unsigned(call) 或类似方法
	// 	OpaqueExtrinsic::default()
	// }

}

impl pallet_babe::Config for Runtime {
	type EpochDuration = EpochDuration;
	type ExpectedBlockTime = ExpectedBlockTime;
	type EpochChangeTrigger = pallet_babe::ExternalTrigger;
	type DisabledValidators = Session;
	type WeightInfo = ();
	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = MaxNominators;
	type KeyOwnerProof = sp_session::MembershipProof;
	type EquivocationReportSystem =
		pallet_babe::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
	// type EquivocationReportSystem = ();
}

impl pallet_authorship::Config for Runtime {
	type FindAuthor = pallet_session::FindAccountFromAuthorIndex<Self, Babe>;
	type EventHandler = (Staking, ImOnline);
}

impl_opaque_keys! {
	pub struct SessionKeys {
		pub grandpa: Grandpa,
		pub babe: Babe,
		pub im_online: ImOnline,
		pub authority_discovery: AuthorityDiscovery,
		// pub mixnet: Mixnet,
		// pub beefy: Beefy,
	}
}

impl pallet_session::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ValidatorId = <Self as frame_system::Config>::AccountId;
	type ValidatorIdOf = sp_runtime::traits::ConvertInto;
	type ShouldEndSession = Babe;
	type NextSessionRotation = Babe;
	type SessionManager = pallet_session::historical::NoteHistoricalRoot<Self, Staking>;
	type SessionHandler = <SessionKeys as OpaqueKeys>::KeyTypeIdProviders;
	type Keys = SessionKeys;
	type DisablingStrategy = pallet_session::disabling::UpToLimitWithReEnablingDisablingStrategy;

	type WeightInfo = pallet_session::weights::SubstrateWeight<Runtime>;
}

impl pallet_session::historical::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type FullIdentification = ();
	type FullIdentificationOf = pallet_staking::UnitIdentificationOf<Self>;
	// type FullIdentificationOf = pallet_staking::NullIdentity;
}

pallet_staking_reward_curve::build! {
	const REWARD_CURVE: PiecewiseLinear<'static> = curve!(
		min_inflation: 0_025_000,
		max_inflation: 0_100_000,
		ideal_stake: 0_500_000,
		falloff: 0_050_000,
		max_piece_count: 40,
		test_precision: 0_005_000,
	);
}

parameter_types! {
	// pub const SessionsPerEra: sp_staking::SessionIndex = 6;
	pub const SessionsPerEra: sp_staking::SessionIndex = 1;
	pub const BondingDuration: sp_staking::EraIndex = 1; //24 * 28;
	pub const SlashDeferDuration: sp_staking::EraIndex = 1; // 24 * 7; // 1/4 the bonding duration.
	pub const RewardCurve: &'static PiecewiseLinear<'static> = &REWARD_CURVE;
	pub const MaxNominators: u32 = 64;
	pub const MaxControllersInDeprecationBatch: u32 = 5900;
	pub OffchainRepeat: BlockNumber = 5;
	pub HistoryDepth: u32 = 84;
}

/// Upper limit on the number of NPOS nominations.
const MAX_QUOTA_NOMINATIONS: u32 = 16;

pub struct StakingBenchmarkingConfig;
impl pallet_staking::BenchmarkingConfig for StakingBenchmarkingConfig {
	type MaxNominators = ConstU32<5000>;
	type MaxValidators = ConstU32<1000>;
}

parameter_types! {
	pub const MaxActiveValidators: u32 = 100;

	pub MaxElectingVotersSolution: u32 = 40_000;

	pub ElectionBoundsOnChain: ElectionBounds = ElectionBoundsBuilder::default()
		.voters_count(5_000.into()).targets_count(1_250.into()).build();
}
pub struct OnChainSeqPhragmen;
impl onchain::Config for OnChainSeqPhragmen {
	type Sort = ConstBool<true>;
	type System = Runtime;
	type Solver = SequentialPhragmen<AccountId, Perbill>;
	type DataProvider = Staking;
	type WeightInfo = frame_election_provider_support::weights::SubstrateWeight<Runtime>;
	type Bounds = ElectionBoundsOnChain;
	
	// type MaxWinners = MaxActiveValidators;
	type MaxBackersPerWinner = MaxElectingVotersSolution;
	type MaxWinnersPerPage = MaxActiveValidators;
}

parameter_types! {
    pub SlashAccount: AccountId = {
        let account_bytes: [u8; 32] = array_bytes::hex2array("24a8fb6a7f226c2dce25e540dcdc6086a42fce9102fb65de2231a8fa4dc52b4e")
            .expect("Invalid hex string for SlashAccount");
        account_bytes.into()
    };
}

impl pallet_staking::Config for Runtime {
	type OldCurrency = Balances;
	type Currency = Balances;
	type CurrencyBalance = Balance;
	type UnixTime = TimestampPallet;
	type CurrencyToVote = sp_staking::currency_to_vote::U128CurrencyToVote;
	// type RewardRemainder = ResolveTo<TreasuryAccount, Balances>;
	type RewardRemainder = ResolveTo<SlashAccount, Balances>;
	type RuntimeEvent = RuntimeEvent;
	type RuntimeHoldReason = RuntimeHoldReason;
	// type Slash = ResolveTo<TreasuryAccount, Balances>; // send the slashed funds to the treasury.
	type Slash = ResolveTo<SlashAccount, Balances>;
	type Reward = (); // rewards are minted from the void
	type SessionsPerEra = SessionsPerEra;
	type BondingDuration = BondingDuration;
	type SlashDeferDuration = SlashDeferDuration;
	/// A super-majority of the council can cancel the slash.
	// type AdminOrigin = EitherOfDiverse<
	// 	EnsureRoot<AccountId>,
	// 	pallet_collective::EnsureProportionAtLeast<AccountId, CouncilCollective, 3, 4>,
	// >;
	type AdminOrigin = EnsureRoot<AccountId>;
	type SessionInterface = Self;
	type EraPayout = pallet_staking::ConvertCurve<RewardCurve>;
	type NextNewSession = Session;
	type MaxExposurePageSize = ConstU32<256>;
	// type ElectionProvider = ElectionProviderMultiPhase;
	type ElectionProvider = onchain::OnChainExecution<OnChainSeqPhragmen>;
	type GenesisElectionProvider = onchain::OnChainExecution<OnChainSeqPhragmen>;
	type VoterList = VoterList;
	
	type NominationsQuota = pallet_staking::FixedNominationsQuota<MAX_QUOTA_NOMINATIONS>;
	// This a placeholder, to be introduced in the next PR as an instance of bags-list
	type TargetList = pallet_staking::UseValidatorsMap<Self>;
	type MaxUnlockingChunks = ConstU32<32>;
	type MaxControllersInDeprecationBatch = MaxControllersInDeprecationBatch;
	type HistoryDepth = HistoryDepth;
	// type EventListeners = (NominationPools, DelegatedStaking);
	type EventListeners = ();
	type WeightInfo = pallet_staking::weights::SubstrateWeight<Runtime>;
	type BenchmarkingConfig = StakingBenchmarkingConfig;
	type Filter = Nothing;
	type MaxValidatorSet = ConstU32<1000>;
}

parameter_types! {
	pub const BagThresholds: &'static [u64] = &voter_bags::THRESHOLDS;
}

type VoterBagsListInstance = pallet_bags_list::Instance1;
impl pallet_bags_list::Config<VoterBagsListInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	/// The voter bags-list is loosely kept up to date, and the real source of truth for the score
	/// of each node is the staking pallet.
	type ScoreProvider = Staking;
	type BagThresholds = BagThresholds;
	type Score = VoteWeight;
	type WeightInfo = pallet_bags_list::weights::SubstrateWeight<Runtime>;
}

parameter_types! {
	pub const ImOnlineUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
	/// We prioritize im-online heartbeats over election solution submission.
	pub const StakingUnsignedPriority: TransactionPriority = TransactionPriority::max_value() / 2;
	pub const MaxAuthorities: u32 = 1000;
	pub const MaxKeys: u32 = 10_000;
	pub const MaxPeerInHeartbeats: u32 = 10_000;
}

impl CreateTransactionBase<pallet_im_online::Call<Runtime>> for Runtime {
	type Extrinsic = sp_runtime::OpaqueExtrinsic;

	type RuntimeCall = RuntimeCall;
}

impl CreateInherent<ImOnlineCall<Runtime>> for Runtime {

	fn create_inherent(_call: Self::RuntimeCall) -> Self::Extrinsic {
		unimplemented!("offchain worker is not supported")
	}

	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		unimplemented!("offchain worker is not supported")
	}

	// fn create_bare(call: RuntimeCall) -> UncheckedExtrinsic {
	// 	UncheckedExtrinsic::new_bare(call)
	// }

	// fn create_bare(
	// 	call: <Self as CreateTransactionBase<pallet_im_online::Call<Runtime>>>::RuntimeCall,
	// ) -> <Self as CreateTransactionBase<pallet_im_online::Call<Runtime>>>::Extrinsic {
	// 	OpaqueExtrinsic::default()
	// }
}

impl pallet_im_online::Config for Runtime {
	type AuthorityId = ImOnlineId;
	type RuntimeEvent = RuntimeEvent;
	type NextSessionRotation = Babe;
	type ValidatorSet = Historical;
	type ReportUnresponsiveness = Offences;
	// type ReportUnresponsiveness = ();
	type UnsignedPriority = ImOnlineUnsignedPriority;
	type WeightInfo = pallet_im_online::weights::SubstrateWeight<Runtime>;
	type MaxKeys = MaxKeys;
	type MaxPeerInHeartbeats = MaxPeerInHeartbeats;
}

impl pallet_offences::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type IdentificationTuple = pallet_session::historical::IdentificationTuple<Self>;
	type OnOffenceHandler = Staking;
}

impl pallet_authority_discovery::Config for Runtime {
	type MaxAuthorities = MaxAuthorities;
}

parameter_types! {
	pub const MaxSetIdSessionEntries: u32 = BondingDuration::get() * SessionsPerEra::get();
}

impl pallet_grandpa::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type MaxAuthorities = MaxAuthorities;
	type MaxNominators = MaxNominators;
	type MaxSetIdSessionEntries = MaxSetIdSessionEntries;
	type KeyOwnerProof = sp_session::MembershipProof;
	// type EquivocationReportSystem =
	// 	pallet_grandpa::EquivocationReportSystem<Self, Offences, Historical, ReportLongevity>;
	type EquivocationReportSystem = ();
}

// impl cumulus_pallet_weight_reclaim::Config for Runtime {
// 	type WeightInfo = ();
// }

#[derive(Clone)]
pub struct TransactionConverter<B>(PhantomData<B>);

impl<B> Default for TransactionConverter<B> {
	fn default() -> Self {
		Self(PhantomData)
	}
}

impl<B: BlockT> fp_rpc::ConvertTransaction<<B as BlockT>::Extrinsic> for TransactionConverter<B> {
	fn convert_transaction(
		&self,
		transaction: pallet_ethereum::Transaction,
	) -> <B as BlockT>::Extrinsic {
		let extrinsic = UncheckedExtrinsic::new_bare(
			pallet_ethereum::Call::<Runtime>::transact { transaction }.into(),
		);
		let encoded = extrinsic.encode();
		<B as BlockT>::Extrinsic::decode(&mut &encoded[..])
			.expect("Encoded extrinsic is always valid")
	}
}

impl pallet_evm_chain_id::Config for Runtime {}
pub struct FindAuthorTruncated<F>(PhantomData<F>);
impl<F: FindAuthor<u32>> FindAuthor<H160> for FindAuthorTruncated<F> {
	fn find_author<'a, I>(digests: I) -> Option<H160>
	where
		I: 'a + IntoIterator<Item = (ConsensusEngineId, &'a [u8])>,
	{
		if let Some(author_index) = F::find_author(digests) {
			let authority_id =
			//	pallet_aura::Authorities::<Runtime>::get()[author_index as usize].clone();
			pallet_session::Validators::<Runtime>::get()[author_index as usize];
			//return Some(H160::from_slice(&authority_id.to_raw_vec()[4..24]));
			return Some(authority_id.into());
		}
		None
	}
}
/// We allow for 2000ms of compute with a 6 second average block time.
pub const WEIGHT_MILLISECS_PER_BLOCK: u64 = 2000;
const BLOCK_GAS_LIMIT: u64 = 75_000_000;
const MAX_POV_SIZE: u64 = 5 * 1024 * 1024;
/// The maximum storage growth per block in bytes.
const MAX_STORAGE_GROWTH: u64 = 400 * 1024;

parameter_types! {
	pub BlockGasLimit: U256 = U256::from(BLOCK_GAS_LIMIT);
	pub const GasLimitPovSizeRatio: u64 = BLOCK_GAS_LIMIT.saturating_div(MAX_POV_SIZE);
	pub const GasLimitStorageGrowthRatio: u64 = BLOCK_GAS_LIMIT.saturating_div(MAX_STORAGE_GROWTH);
	pub PrecompilesValue: FrontierPrecompiles<Runtime> = FrontierPrecompiles::<_>::new();
	pub WeightPerGas: Weight = Weight::from_parts(weight_per_gas(BLOCK_GAS_LIMIT, NORMAL_DISPATCH_RATIO, WEIGHT_MILLISECS_PER_BLOCK), 0);
}

impl pallet_evm::Config for Runtime {
	type AccountProvider = pallet_evm::FrameSystemAccountProvider<Self>;
	type FeeCalculator = BaseFee;
	type GasWeightMapping = pallet_evm::FixedGasWeightMapping<Self>;
	type WeightPerGas = WeightPerGas;
	type BlockHashMapping = pallet_ethereum::EthereumBlockHashMapping<Self>;
	type CallOrigin = EnsureAccountId20;
	type WithdrawOrigin = EnsureAccountId20;
	type AddressMapping = IdentityAddressMapping;
	type Currency = Balances;
	type PrecompilesType = FrontierPrecompiles<Self>;
	type PrecompilesValue = PrecompilesValue;
	type ChainId = EVMChainId;
	type BlockGasLimit = BlockGasLimit;
	type Runner = pallet_evm::runner::stack::Runner<Self>;
	type OnChargeTransaction = ();
	type OnCreate = ();
	type FindAuthor = FindAuthorTruncated<Babe>;
	type GasLimitPovSizeRatio = GasLimitPovSizeRatio;
	type GasLimitStorageGrowthRatio = GasLimitStorageGrowthRatio;
	type Timestamp = TimestampPallet;
	type CreateOriginFilter = ();
	type CreateInnerOriginFilter = ();
	type WeightInfo = pallet_evm::weights::SubstrateWeight<Self>;
}

parameter_types! {
	pub const PostBlockAndTxnHashes: PostLogContent = PostLogContent::BlockAndTxnHashes;
}

impl pallet_ethereum::Config for Runtime {
	type StateRoot = pallet_ethereum::IntermediateStateRoot<Self::Version>;
	type PostLogContent = PostBlockAndTxnHashes;
	type ExtraDataLength = ConstU32<30>;
}


parameter_types! {
	pub BoundDivision: U256 = U256::from(1024);
}

impl pallet_dynamic_fee::Config for Runtime {
	type MinGasPriceBoundDivisor = BoundDivision;
}

parameter_types! {
	pub DefaultBaseFeePerGas: U256 = U256::from(1_000_000_000);
	pub DefaultElasticity: Permill = Permill::from_parts(125_000);
}
pub struct BaseFeeThreshold;
impl pallet_base_fee::BaseFeeThreshold for BaseFeeThreshold {
	fn lower() -> Permill {
		Permill::zero()
	}
	fn ideal() -> Permill {
		Permill::from_parts(500_000)
	}
	fn upper() -> Permill {
		Permill::from_parts(1_000_000)
	}
}
impl pallet_base_fee::Config for Runtime {
	type Threshold = BaseFeeThreshold;
	type DefaultBaseFeePerGas = DefaultBaseFeePerGas;
	type DefaultElasticity = DefaultElasticity;
}