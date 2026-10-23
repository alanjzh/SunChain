//! Mock runtime used by the `pallet-exchange-core` unit tests.

use crate as pallet_exchange_core;
use codec::{Decode, Encode};
use exchange_primitives::{MarketId, PriceOracle, PriceTicks};
use frame_support::{
    derive_impl, parameter_types,
    traits::{AsEnsureOriginWithArg, ConstI64, ConstU128, ConstU32, Get},
    weights::Weight,
    PalletId,
};
use frame_system::{EnsureRoot, EnsureSigned};
use sp_runtime::traits::{IdentifyAccount, Verify};
use sp_runtime::{
    traits::{AccountIdConversion, IdentityLookup},
    BuildStorage,
};

pub type AccountId = u64;
pub type Balance = u128;
pub type AssetId = u32;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
    pub enum Test {
        System: frame_system,
        Balances: pallet_balances,
        Assets: pallet_assets,
        ExchangeCore: pallet_exchange_core,
        Insurance: pallet_insurance,
        Liquidation: pallet_liquidation,
    }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
    type Block = Block;
    type AccountId = AccountId;
    type Lookup = IdentityLookup<AccountId>;
    type AccountData = pallet_balances::AccountData<Balance>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
    type Balance = Balance;
    type ExistentialDeposit = ConstU128<1>;
    type AccountStore = System;
}

impl pallet_assets::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Balance = Balance;
    type RemoveItemsLimit = ConstU32<1000>;
    type AssetId = AssetId;
    type AssetIdParameter = AssetId;
    type Currency = Balances;
    type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<AccountId>>;
    type ForceOrigin = EnsureRoot<AccountId>;
    type AssetDeposit = ConstU128<0>;
    type AssetAccountDeposit = ConstU128<0>;
    type MetadataDepositBase = ConstU128<0>;
    type MetadataDepositPerByte = ConstU128<0>;
    type ApprovalDeposit = ConstU128<0>;
    type StringLimit = ConstU32<50>;
    type Freezer = ();
    type Holder = ();
    type Extra = ();
    type CallbackHandle = ();
    type WeightInfo = ();
}

parameter_types! {
    pub const ExchangePalletId: PalletId = PalletId(*b"sun/exch");
    // Must NOT share its first 4 bytes with `ExchangePalletId`: `into_account_truncating` on a
    // `u64` AccountId only keeps the leading 8 bytes of the derivation input, so two `b"sun…"`
    // ids collapse into the same account and the fund would pay out to itself.
    pub const InsurancePalletId: PalletId = PalletId(*b"insfund0");
    pub const CollateralAssetId: AssetId = 1;
    pub const MaxMarkets: u32 = 3;
    pub const MaxSubaccountsPerAccount: u32 = 16;
    pub const MaxClearingMarkets: u32 = 2;
    /// Small on purpose: the ring's eviction is part of what is under test (S1-5).
    pub const MaxReceiptsPerAccount: u32 = 4;
    pub const MaxOpenOrdersPerAccount: u32 = 16;
    pub const MaxPriceLevelsPerSide: u32 = 32;
    pub const MaxOrdersPerLevel: u32 = 16;
    pub const MaxMatchesPerAction: u32 = 8;
    pub const MaxActionsPerBatch: u32 = 8;
    pub const MaxPrunePerCall: u32 = 16;
    pub const MaxOrderLifetime: u64 = 28_800;
    // Liquidation (§8.2 / §8.3). Uncalibrated placeholders, like the other Phase 2 constants.
    pub const MaintenanceBufferPpm: u32 = 10_000;
    pub const LiquidationPenaltyPpm: u32 = 5_000;
    pub const LiquidationKeeperRebatePpm: u32 = 1_000;
    pub const MaxLiquidationSlippagePpm: u32 = 50_000;
    pub const MinLiquidationLots: u64 = 1;
    // Funding: sample every 60 blocks (3 min), tick every 1_200 blocks (1 h), and a full
    // rate is charged over 9_600 blocks (8 h) — i.e. a 1 h tick charges 1/8 of the rate.
    pub const FundingSamplePeriod: u64 = 60;
    pub const FundingTickPeriod: u64 = 1_200;
    pub const FundingRatePeriod: u64 = 9_600;
    pub const MaxPremiumSamples: u32 = 64;
    pub const MaxPremiumVotes: u32 = 32;
    pub const RemovedTailSampleRatioPpm: u32 = 50_000;
    pub const DefaultFundingPpm: i32 = 0;
    pub const FundingRateClampFactorPpm: u32 = 600_000;
    pub const MaxAbsPremiumPpm: u32 = 60_000;
    /// 500 USDC (6 decimals) of impact notional, as in dYdX.
    pub const ImpactNotionalQuote: u128 = 500_000_000;
    pub const MaxExchangeActionsPerBlock: u32 = 1_000;
    /// Effectively unlimited: the budget itself is covered by dedicated tests.
    pub ExchangeBlockWeightBudget: Weight = Weight::from_parts(u64::MAX, u64::MAX);
}

mod test_sig {
    use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
    use scale_info::TypeInfo;
    use sp_core::crypto::Pair as _;
    use sp_core::ecdsa;
    use sp_runtime::traits::{IdentifyAccount, Verify};

    use super::AccountId;

    /// The test double's key for an account.
    ///
    /// Deterministic, so a test can sign as anyone without carrying key material around.
    /// Real signatures are the point: `TestSignature` used to ignore the message entirely,
    /// so nothing about the signed payload — the domain, the version, the chain it was signed
    /// on, the expiry — was ever actually tested, only who *claimed* to sign. A payload
    /// change was therefore invisible to the whole suite.
    pub fn keyring(account: AccountId) -> ecdsa::Pair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&account.to_le_bytes());
        ecdsa::Pair::from_seed(&seed)
    }

    /// Signs `payload` as `account`, over the keccak digest the runtime re-derives.
    pub fn test_sign(account: AccountId, payload: &[u8]) -> TestSignature {
        TestSignature(account, keyring(account).sign_prehashed(&sp_io::hashing::keccak_256(payload)))
    }

    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Encode,
        Decode,
        DecodeWithMemTracking,
        TypeInfo,
        MaxEncodedLen,
    )]
    pub struct TestSigner(pub AccountId);

    impl IdentifyAccount for TestSigner {
        type AccountId = AccountId;
        fn into_account(self) -> AccountId {
            self.0
        }
    }

    /// `(claimed signer, signature)`. The production runtime uses `MultiSignature` /
    /// `MultiSigner`; this keeps the same shape with a key the test controls.
    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Encode,
        Decode,
        DecodeWithMemTracking,
        TypeInfo,
        MaxEncodedLen,
    )]
    pub struct TestSignature(pub AccountId, pub ecdsa::Signature);

    impl Verify for TestSignature {
        type Signer = TestSigner;
        fn verify<L: sp_runtime::traits::Lazy<[u8]>>(&self, mut msg: L, signer: &AccountId) -> bool {
            if self.0 != *signer {
                return false
            }
            // Recover over the message the caller passed: a signature made over *other*
            // bytes does not verify here, which is the whole point of signing a payload
            // rather than an identity.
            self.1
                .recover_prehashed(&sp_io::hashing::keccak_256(msg.get()))
                .is_some_and(|recovered| recovered == keyring(*signer).public())
        }
    }
}

pub use test_sig::{keyring, test_sign, TestSignature, TestSigner};


/// A price oracle the tests can set, standing in for `pallet-oracle`.
///
/// This exists because the core no longer keeps a Root-set cached price: with `NoOracle`
/// there is no mark price at all, so every margin, liquidation and funding test would fail.
/// The cache used to paper over that, which is exactly why removing it needed this first.
pub struct TestOracle;

/// Where the test prices live.
///
/// Deliberately *not* a process-wide static: tests run in parallel and would clobber each
/// other's price, which shows up as failures that appear and disappear. This lives in
/// externalities, so every test gets its own.
const ORACLE_KEY: &[u8] = b":sun:test-oracle:";

impl TestOracle {
	fn read() -> Vec<(MarketId, PriceTicks)> {
		sp_io::storage::get(ORACLE_KEY)
			.and_then(|raw| codec::Decode::decode(&mut &raw[..]).ok())
			.unwrap_or_default()
	}

	fn write(prices: &[(MarketId, PriceTicks)]) {
		sp_io::storage::set(ORACLE_KEY, &prices.encode());
	}

	pub fn set(market: MarketId, price: PriceTicks) {
		let mut prices = Self::read();
		prices.retain(|(m, _)| *m != market);
		prices.push((market, price));
		Self::write(&prices);
	}

	pub fn clear() {
		Self::write(&[]);
	}

	fn get(market: MarketId) -> Option<PriceTicks> {
		Self::read().iter().find(|(m, _)| *m == market).map(|(_, p)| *p)
	}
}

impl PriceOracle<MarketId> for TestOracle {
	fn index_price(market: MarketId) -> Option<PriceTicks> {
		Self::get(market)
	}
	fn mark_price(market: MarketId) -> Option<PriceTicks> {
		Self::get(market)
	}
	fn is_fresh(market: MarketId) -> bool {
		Self::get(market).is_some()
	}
}

impl pallet_exchange_core::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Assets = Assets;
    type Signature = TestSignature;
    type Signer = TestSigner;
    type AssetId = AssetId;
    type Balance = Balance;
    type ForceOrigin = EnsureRoot<AccountId>;
    type PalletId = ExchangePalletId;
    type CollateralAssetId = CollateralAssetId;
    type MaxMarkets = MaxMarkets;
    type MaxSubaccountsPerAccount = MaxSubaccountsPerAccount;
    type MaxClearingMarkets = MaxClearingMarkets;
    type MaxReceiptsPerAccount = MaxReceiptsPerAccount;
    type MaxOpenOrdersPerAccount = MaxOpenOrdersPerAccount;
    type MaxPriceLevelsPerSide = MaxPriceLevelsPerSide;
    type MaxOrdersPerLevel = MaxOrdersPerLevel;
    type MaxMatchesPerAction = MaxMatchesPerAction;
    type MaxActionsPerBatch = MaxActionsPerBatch;
    type MaxPrunePerCall = MaxPrunePerCall;
    type MaxOrderLifetime = MaxOrderLifetime;
    type MaintenanceBufferPpm = MaintenanceBufferPpm;
    type LiquidationPenaltyPpm = LiquidationPenaltyPpm;
    type LiquidationKeeperRebatePpm = LiquidationKeeperRebatePpm;
    type MaxLiquidationSlippagePpm = MaxLiquidationSlippagePpm;
    type MinLiquidationLots = MinLiquidationLots;
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
    type ExchangeBlockWeightBudget = ExchangeBlockWeightBudget;
    /// No oracle in Phase 1; the core falls back to the Root-set cached mark price.
    type PriceOracle = TestOracle;
    type Insurance = Insurance;
    type WeightInfo = ();
}

/// The exchange core custody account: where the insurance fund pays out to.
pub struct ExchangeCustody;

impl Get<AccountId> for ExchangeCustody {
    fn get() -> AccountId {
        ExchangePalletId::get().into_account_truncating()
    }
}

impl pallet_liquidation::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Engine = ExchangeCore;
    type MaxLiquidationsPerCall = ConstU32<8>;
    type MinAdlRoePpm = ConstI64<50_000>;
    type AdlPriceBandPpm = ConstU32<1_000>;
    type WeightInfo = ();
}

impl pallet_insurance::Config for Test {
    type RuntimeEvent = RuntimeEvent;
    type Assets = Assets;
    type AssetId = AssetId;
    type Balance = Balance;
    type PalletId = InsurancePalletId;
    type CollateralAssetId = CollateralAssetId;
    type Beneficiary = ExchangeCustody;
}

/// Test USDC asset id.
pub const USDC: AssetId = 1;
/// The only market of Phase 1.
pub const MARKET: crate::types::MarketId = 0;

pub fn new_test_ext() -> sp_io::TestExternalities {
    let t = frame_system::GenesisConfig::<Test>::default()
        .build_storage()
        .unwrap();
    let mut ext: sp_io::TestExternalities = t.into();
    ext.execute_with(|| {
        System::set_block_number(1);
        TestOracle::clear();
        // Tier 0 is what `market_config()` points at. Margins now live on the tier, so the
        // tests need one to exist.
        crate::LiquidityTiers::<Test>::insert(
            0,
            crate::LiquidityTier {
                name: *b"test    ",
                initial_margin_ppm: 50_000,
                maintenance_margin_ppm: 25_000,
                impact_notional_quote: 500_000_000,
            },
        );
        frame_support::assert_ok!(Assets::force_create(
            RuntimeOrigin::root(),
            USDC.into(),
            1,
            true,
            1,
        ));
    });
    ext
}
