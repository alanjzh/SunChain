//! Mock runtime used by the `pallet-oracle` unit tests.
//!
//! The pallet had no mock at all, which meant the whitelist, the per-source nonce and the
//! aggregation hook — the path an external price feed actually goes through — were
//! completely untested. Only the two pure helpers in `types.rs` had tests.

use crate as pallet_oracle;
use frame_support::{derive_impl, parameter_types, traits::ConstU128};
use frame_system::EnsureRoot;
use exchange_primitives::MarketId;
use codec::{Decode, Encode};
use sp_runtime::{traits::IdentityLookup, BuildStorage};

pub type AccountId = u64;
pub type Balance = u128;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Oracle: pallet_oracle,
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

parameter_types! {
	pub const MaxSources: u32 = 8;
	/// Two sources must agree before a price exists. dYdX `MinExchanges`.
	pub const MinSources: u32 = 2;
	pub const MaxMarkets: u32 = 16;
	/// A quote older than 10 blocks no longer counts toward the quorum.
	pub const MaxPriceAge: u64 = 10;
	/// Deliberately non-zero: this gate belongs to the aggregation path and deserves a test
	/// through it, not only through its pure helper.
	pub const MinPriceChangePpm: u32 = 10_000;
}

/// Stand-in for a source's public key: in the mock the "key" is just the account id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo)]
pub struct MockSigner(pub AccountId);

impl sp_runtime::traits::IdentifyAccount for MockSigner {
	type AccountId = AccountId;
	fn into_account(self) -> AccountId {
		self.0
	}
}

/// A signature over a vote payload.
///
/// Deliberately **not cryptographic** — `verify` just recomputes the digest the "signer"
/// would have produced. That is enough to prove the payload is bound to the vote and that a
/// signature that does not match is rejected, which is what this pallet is responsible for.
/// Cryptographic strength comes from the real scheme the runtime plugs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, codec::Encode, codec::Decode, codec::DecodeWithMemTracking, scale_info::TypeInfo)]
pub struct MockSignature(pub [u8; 32]);

impl MockSigner {
	pub fn sign(self, payload: &[u8]) -> MockSignature {
		MockSignature(sp_io::hashing::blake2_256(
			&(self.0, payload.to_vec()).encode(),
		))
	}
}

impl sp_runtime::traits::Verify for MockSignature {
	type Signer = MockSigner;
	fn verify<L: sp_runtime::traits::Lazy<[u8]>>(&self, mut msg: L, signer: &AccountId) -> bool {
		self.0 == sp_io::hashing::blake2_256(&(*signer, msg.get().to_vec()).encode())
	}
}

impl pallet_oracle::Config for Test {
	type Signature = MockSignature;
	type Signer = MockSigner;
	type RuntimeEvent = RuntimeEvent;
	type ForceOrigin = EnsureRoot<AccountId>;
	type MaxSources = MaxSources;
	type MinSources = MinSources;
	type MaxMarkets = MaxMarkets;
	type MaxPriceAge = MaxPriceAge;
	type MinPriceChangePpm = MinPriceChangePpm;
	// No `weights.rs` exists for this pallet; the unit computes trivial weights.
	type WeightInfo = crate::Pallet<Test>;
}

/// Default genesis: no sources, no tracked markets.
pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = RuntimeGenesisConfig::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Genesis with sources and tracked markets configured, which is what a real chain needs:
/// with the default genesis nothing can publish at all until governance acts.
pub fn new_test_ext_with(
	sources: Vec<AccountId>,
	tracked_markets: Vec<MarketId>,
) -> sp_io::TestExternalities {
	let t = RuntimeGenesisConfig {
		oracle: crate::GenesisConfig { sources, tracked_markets },
		..Default::default()
	}
	.build_storage()
	.unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}
