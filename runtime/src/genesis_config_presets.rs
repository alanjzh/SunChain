use crate::{
	AccountId, Balance, BalancesConfig, OracleConfig, RuntimeGenesisConfig, SudoConfig,
	UNIT,
};
use alloc::{vec, vec::Vec};
use frame_support::build_struct_json_patch;
use serde_json::Value;
use sp_consensus_aura::sr25519::{self, AuthorityId as AuraId};
use sp_consensus_grandpa::AuthorityId as GrandpaId;
use sp_core::{crypto::Ss58Codec, ed25519, sr25519 as core_sr25519, Pair, Public};
use sp_genesis_builder::{self, PresetId};
use exchange_primitives::MarketId;
use sp_keyring::Sr25519Keyring;
use hex_literal::hex;

pub const SUNCHAIN_RUNTIME_PRESET: &'static str = "sunchain";
// pub static SUNCHAIN_COMMUNITY: AccountId = 
// 	array_bytes::hex_n_into_unchecked("24a8fb6a7f226c2dce25e540dcdc6086a42fce9102fb65de2231a8fa4dc52b4e");
// Returns the genesis config presets populated with given parameters.
fn testnet_genesis(
	initial_authorities: Vec<(AuraId, GrandpaId)>,
	endowed_accounts: Vec<AccountId>,
	root: AccountId,
) -> Value {
	build_struct_json_patch!(RuntimeGenesisConfig {
		balances: BalancesConfig {
			balances: endowed_accounts
				.iter()
				.cloned()
				// .map(|k| (k, 1u128 << 60))
				.map(|k| (k, 100_000_000u128 * UNIT))
				.collect::<Vec<_>>(),
		},
		// aura: pallet_aura::GenesisConfig {
		// 	authorities: initial_authorities.iter().map(|x| (x.0.clone())).collect::<Vec<_>>(),
		// },
		grandpa: pallet_grandpa::GenesisConfig {
			authorities: initial_authorities.iter().map(|x| (x.1.clone(), 1)).collect::<Vec<_>>(),
		},
		sudo: SudoConfig { key: Some(root) },
		oracle: OracleConfig {
			// A dev-only convenience: the feed signs with a well-known development key, so
			// the index price exists on a fresh chain without governance having to act first.
			// A real chain must run its own key here.
			sources: vec![AccountId::from(hex!(
				"773539d4Ac0e786233D90A233654ccEE26a613D9"
			))], // Dorothy
			// No markets are created at genesis, so this starts empty; `track_market` adds
			// them as they are listed.
			tracked_markets: vec![] as Vec<MarketId>,
		},
	})
}

/// Return the development genesis config.
pub fn development_config_genesis() -> Value {
	testnet_genesis(
		vec![(
			sp_keyring::Sr25519Keyring::Alice.public().into(),
			sp_keyring::Ed25519Keyring::Alice.public().into(),
		)],
		vec![
				AccountId::from(hex!("f24FF3a9CF04c71Dbc94D0b566f7A27B94566cac")), // Alith
				AccountId::from(hex!("3Cd0A705a2DC65e5b1E1205896BaA2be8A07c6e0")), // Baltathar
				AccountId::from(hex!("798d4Ba9baf0064Ec19eB4F0a1a45785ae9D6DFc")), // Charleth
				AccountId::from(hex!("773539d4Ac0e786233D90A233654ccEE26a613D9")), // Dorothy
				AccountId::from(hex!("Ff64d3F6efE2317EE2807d223a0Bdc4c0c49dfDB")), // Ethan
				AccountId::from(hex!("C0F0f4ab324C46e55D02D0033343B4Be8A55532d")), // Faith
			],
			AccountId::from(hex!("f24FF3a9CF04c71Dbc94D0b566f7A27B94566cac")),
	)
}

/// Return the local genesis config preset.
pub fn local_config_genesis() -> Value {
	testnet_genesis(
		vec![
			(
				sp_keyring::Sr25519Keyring::Alice.public().into(),
				sp_keyring::Ed25519Keyring::Alice.public().into(),
			),
			(
				sp_keyring::Sr25519Keyring::Bob.public().into(),
				sp_keyring::Ed25519Keyring::Bob.public().into(),
			),
		],
		// Sr25519Keyring::iter()
		// 	.filter(|v| v != &Sr25519Keyring::One && v != &Sr25519Keyring::Two)
		// 	.map(|v| v.to_account_id())
		// 	.collect::<Vec<_>>(),
		Sr25519Keyring::iter()
		.filter(|v| v != &Sr25519Keyring::One && v != &Sr25519Keyring::Two)
		.map(|v| {
			// 将 Sr25519 公钥哈希到 20 字节以匹配 AccountId20
			let public_key = v.to_raw_public();
			let eth_address : [u8; 20]= sp_io::hashing::blake2_256(&public_key)[0..20].try_into().unwrap();
			AccountId::from(eth_address)
		})
		.collect::<Vec<_>>(),
			AccountId::from(hex!("f24FF3a9CF04c71Dbc94D0b566f7A27B94566cac"))
	)
}

pub fn sunchain_config_genesis() -> Value {
//  Sunchain Admin Master：	
// 	Secret phrase:       immune dirt melody hour say remind era vivid oblige hope issue breeze
//   Network ID:        substrate
//   Secret seed:       0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a
//   Public key (hex):  0xf4f928abe7bcfd1f243aafdf100daf414490bc8f85deeeaf93c87f15b7b4ef5d
//   Account ID:        0xf4f928abe7bcfd1f243aafdf100daf414490bc8f85deeeaf93c87f15b7b4ef5d
//   Public key (SS58): 5HbuZEuQ8My5Q6ZRPupFa1cwCp456ZtxaxQHhQLqRxKAdZFx
//   SS58 Address:      5HbuZEuQ8My5Q6ZRPupFa1cwCp456ZtxaxQHhQLqRxKAdZFx
// key inspect 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --scheme ed25519
// Secret Key URI `0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a` is account:
//   Network ID:        substrate
//   Secret seed:       0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a
//   Public key (hex):  0x79a1aaefdb8f199dc8ce2fb9b93926835ed16fe345207f48f6191d440e98b784
//   Account ID:        0x79a1aaefdb8f199dc8ce2fb9b93926835ed16fe345207f48f6191d440e98b784
//   Public key (SS58): 5EpBgCBanmGuRURgmgEBUYNSALts5axhyFvGAFmNDFQacVSr
//   SS58 Address:      5EpBgCBanmGuRURgmgEBUYNSALts5axhyFvGAFmNDFQacVSr

//  Sunchain Admin Slave：
// Secret phrase:       toast vital rocket ankle awake trust model love sudden rebuild hazard cloth
//   Network ID:        substrate
//   Secret seed:       0x26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a
//   Public key (hex):  0x485a34a7a5b745adce063c2a58db00d2b6ff0ea56fbb4884861521f53c47870e
//   Account ID:        0x485a34a7a5b745adce063c2a58db00d2b6ff0ea56fbb4884861521f53c47870e
//   Public key (SS58): 5Dha6qr9sbURXuwqmVe3tKgPDXMrAciVrrNmG3eosg211Wyk
//   SS58 Address:      5Dha6qr9sbURXuwqmVe3tKgPDXMrAciVrrNmG3eosg211Wyk

// inspect --scheme ed25519 0x26712bfe38c72e6532ef36dbdff4c1c269ff697
// 8bbd3204320159864c3fcc88a
// Secret Key URI `0x26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a` is account:
//   Network ID:        substrate
//   Secret seed:       0x26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a
//   Public key (hex):  0xdfd4cf92fc1dbd8292a021957216a1b8b7f145c708a01fba074caccc4aec2846
//   Account ID:        0xdfd4cf92fc1dbd8292a021957216a1b8b7f145c708a01fba074caccc4aec2846
//   Public key (SS58): 5H8BkewEzXsGoH99UpqP4CxveH8gWWgkGkXzVgN1ah2yGjd3
//   SS58 Address:      5H8BkewEzXsGoH99UpqP4CxveH8gWWgkGkXzVgN1ah2yGjd3

//  Sunchain Community：
// Secret phrase:       machine keep else assist mule jump girl razor mimic there vessel kid
//   Network ID:        substrate
//   Secret seed:       0xf46633926f2b0eb7b14b8b755299070e6aa5d4538af4618f7871168cb74200ce
//   Public key (hex):  0x24a8fb6a7f226c2dce25e540dcdc6086a42fce9102fb65de2231a8fa4dc52b4e
//   Account ID:        0x24a8fb6a7f226c2dce25e540dcdc6086a42fce9102fb65de2231a8fa4dc52b4e
//   Public key (SS58): 5Ctmn9QR344big7bj5LuSunGJ78gV9qEJAZpzAaJJwHs1KpQ
//   SS58 Address:      5Ctmn9QR344big7bj5LuSunGJ78gV9qEJAZpzAaJJwHs1KpQ
	let sun_admin_master_sr25519 = core_sr25519::Public::from_ss58check(
		"5HbuZEuQ8My5Q6ZRPupFa1cwCp456ZtxaxQHhQLqRxKAdZFx"
	).unwrap();
	let sun_admin_master_ed25519 = ed25519::Public::from_ss58check(
		"5EpBgCBanmGuRURgmgEBUYNSALts5axhyFvGAFmNDFQacVSr"
	).unwrap();

	let sun_admin_slave_sr25519 = core_sr25519::Public::from_ss58check(
		"5Dha6qr9sbURXuwqmVe3tKgPDXMrAciVrrNmG3eosg211Wyk"
	).unwrap();
	let sun_admin_slave_ed25519 = ed25519::Public::from_ss58check(
		"5H8BkewEzXsGoH99UpqP4CxveH8gWWgkGkXzVgN1ah2yGjd3"
	).unwrap();

	let sun_community_sr25519 = core_sr25519::Public::from_ss58check(
		"5Ctmn9QR344big7bj5LuSunGJ78gV9qEJAZpzAaJJwHs1KpQ"
	).unwrap();

	// let sun_admin_master_account = AccountId::from(sun_admin_master_sr25519.clone());
	// let sun_community_account = AccountId::from(sun_community_sr25519.clone());

	let sun_admin_master_account = AccountId::from(<[u8; 32]>::from(sun_admin_master_sr25519.clone()));
	let sun_community_account = AccountId::from(<[u8; 32]>::from(sun_community_sr25519.clone()));

	testnet_genesis(
		vec![
			(
				sun_admin_master_sr25519.into(),
				sun_admin_master_ed25519.into(),			
			),
			(
				sun_admin_slave_sr25519.into(),
				sun_admin_slave_ed25519.into(),			
			),
		],
		// 
		vec![
			sun_admin_master_account.clone(),
			sun_community_account,
		],
		sun_admin_master_account,
	)
}

/// Provides the JSON representation of predefined genesis config for given `id`.
pub fn get_preset(id: &PresetId) -> Option<Vec<u8>> {
	let patch = match id.as_ref() {
		sp_genesis_builder::DEV_RUNTIME_PRESET => development_config_genesis(),
		sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET => local_config_genesis(),
		SUNCHAIN_RUNTIME_PRESET => sunchain_config_genesis(),
		_ => return None,
	};
	Some(
		serde_json::to_string(&patch)
			.expect("serialization to json is expected to work. qed.")
			.into_bytes(),
	)
}

/// List of supported presets.
pub fn preset_names() -> Vec<PresetId> {
	vec![
		PresetId::from(sp_genesis_builder::DEV_RUNTIME_PRESET),
		PresetId::from(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET),
		PresetId::from(SUNCHAIN_RUNTIME_PRESET),
	]
}
