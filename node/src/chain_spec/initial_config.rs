
use std::collections::HashMap;

use sunchain_runtime::BABE_GENESIS_EPOCH_CONFIG;
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_babe::AuthorityId as BabeId;
use sp_consensus_beefy::ecdsa_crypto;
use sp_consensus_grandpa::AuthorityId as GrandpaId;
use sunlead_primitives::SunLeadId;
use sp_core::{
	crypto::{get_public_from_string_or_panic, CryptoType, Ss58Codec},
	ed25519, sr25519 as core_sr25519, Pair, H160, } ;
use sp_staking::{
	StakerStatus,
};
use sp_runtime::Perbill;
use sp_tracing::{info, warn, debug};

pub use sunchain_runtime::{AccountId, Balance, UNIT, configs::SessionKeys};
pub const ENDOWMENT: Balance = 100_000_000 * UNIT;
pub const STASH: Balance = ENDOWMENT / 100;

/// The staker type as supplied ot the Staking config.
pub type Staker = (AccountId, AccountId, Balance, StakerStatus<AccountId>);

fn eth_address_to_account_id(eth_address: &str) -> AccountId {
	let address: H160 = eth_address
		.parse()
		.expect("invalid Ethereum address");
	AccountId::from(address)
}


fn configure_accounts_for_staging_sunchain() -> (
	Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		SunLeadId,
	)>,
	AccountId,
	Vec<AccountId>,
) {
	//private key: 0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef
	// let eth_address: H160 = "0x1be31a94361a391bbafb2a4ccd704f57dc04d4bb"
    // .parse()
    // .expect("valid H160");

// 转换成 AccountId20
	// let account = AccountId::from(eth_address);

	let account = eth_address_to_account_id("0x1be31a94361a391bbafb2a4ccd704f57dc04d4bb");
	// cast wallet addr --mnemonic "immune dirt melody hour say remind era vivid oblige hope issue breeze" --mnemonic-derivation-path "m/44'/60'/0'/0/0"
	let sun_admin_master_account = eth_address_to_account_id("0x6dE2B57754235cA8f4f7464CF829Ba0BF1456B66");
	// cast wallet addr --mnemonic "toast vital rocket ankle awake trust model love sudden rebuild hazard cloth" --mnemonic-derivation-path "m/44'/60'/0'/0/0"
	let sun_admin_slave_account = eth_address_to_account_id("0x81A558A11EA36559a73B6B35FD74352B536f9736");
	// cast wallet addr --mnemonic "machine keep else assist mule jump girl razor mimic there vessel kid" --mnemonic-derivation-path "m/44'/60'/0'/0/0"
	let sun_community_account = eth_address_to_account_id("0x9bE983986a4Eb2eD9E03637308b29ADb4BFd9961");
	
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
	// let sun_admin_slave_account = AccountId::from(sun_admin_slave_sr25519.clone());
	// let sun_community_account = AccountId::from(sun_community_sr25519.clone());

	// let sun_admin_master_account = AccountId::from(sun_admin_master_sr25519.clone().0);
	// let sun_admin_slave_account = AccountId::from(sun_admin_slave_sr25519.clone().0);
	// let sun_community_account = AccountId::from(sun_community_sr25519.clone().0);

	// let (sun_admin_master_stash, sun_admin_master_controller, sun_admin_master_session_keys) =
	// 	authority_keys_from_seed("74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a");
	// let (sun_admin_slave_stash, sun_admin_slave_controller, sun_admin_slave_session_keys) =
	// 	authority_keys_from_seed("26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a");
	// let (_sun_community_stash, sun_community_controller, _sun_community_session_keys) =
	// 	authority_keys_from_seed("f46633926f2b0eb7b14b8b755299070e6aa5d4538af4618f7871168cb74200ce");	

	let initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		SunLeadId,
	)> = vec![
		// Two validators, because a single-validator chain cannot demonstrate any of the
		// consensus behaviour — no handover, no equivocation surface — and because `MinSources`
		// aside, block production halts entirely if that one node stops.
		(
			sun_admin_slave_account.clone(),
			sun_admin_slave_account.clone().into(),
			sun_admin_slave_ed25519.into(),
			sun_admin_slave_sr25519.clone().into(),
			sun_admin_slave_sr25519.clone().into(),
			sun_admin_slave_sr25519.clone().into(),
			sun_admin_slave_sr25519.into(),
		),
		( 
			sun_admin_master_account.clone(),
			sun_admin_master_account.clone().into(),
			sun_admin_master_ed25519.into(),
			sun_admin_master_sr25519.clone().into(),
			sun_admin_master_sr25519.clone().into(),
			sun_admin_master_sr25519.clone().into(),
			sun_admin_master_sr25519.into(),       
		),
	];

	// let root_key: AccountId = array_bytes::hex_n_into_unchecked(
	// 	// 5HbuZEuQ8My5Q6ZRPupFa1cwCp456ZtxaxQHhQLqRxKAdZFx
	// 	"f4f928abe7bcfd1f243aafdf100daf414490bc8f85deeeaf93c87f15b7b4ef5d",
	// );
	let root_key	= sun_admin_master_account.clone();

	let endowed_accounts: Vec<AccountId> = vec![
		account.into(),
		sun_community_account.into(),
		sun_admin_master_account.clone().into(),
		sun_admin_slave_account.into(),
	];

	warn!(target: "sunchain", "initial_authorities, : {:?}  endowed_accounts : {:?}", initial_authorities, endowed_accounts);
	(initial_authorities, root_key, endowed_accounts)
}

/// Configure the accounts for the testnet.
///
/// * Adds `initial_authorities` and `initial_nominators` to endowed accounts if missing.
/// * Sets up the stakers consisting of the `initial_authorities` and `initial_nominators`.
fn configure_accounts(
	initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		SunLeadId,
	)>,
	initial_nominators: Vec<AccountId>,
	endowed_accounts: Vec<AccountId>,
	stash: HashMap<AccountId,Balance>,
) -> (
	Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		SunLeadId,
	)>,
	Vec<AccountId>,
	Vec<Staker>,
) {
	// let mut endowed_accounts = endowed_accounts;
	// // endow all authorities and nominators.
	// initial_authorities
	// 	.iter()
	// 	.map(|x| &x.0)
	// 	.chain(initial_nominators.iter())
	// 	.for_each(|x| {
	// 		if !endowed_accounts.contains(x) {
	// 			endowed_accounts.push(x.clone())
	// 		}
	// 	});

	// stakers: all validators and nominators.

	let stakers = initial_authorities
		.iter()
		.map(|x| (x.0.clone(), x.0.clone(), *stash.get(&x.0).unwrap_or(&0u128), StakerStatus::Validator))
		.chain(initial_nominators.iter().map(|x| {
			
			let nominations = initial_authorities
				.as_slice()
				.into_iter()
				.map(|choice| choice.0.clone())
				.collect::<Vec<_>>();
			(x.clone(), x.clone(), *stash.get(&x).unwrap_or(&0u128), StakerStatus::Nominator(nominations))
		}))
		.collect::<Vec<_>>();

	(initial_authorities, endowed_accounts, stakers)
}

/// Creates the session keys as defined by the runtime.
fn session_keys_json(
	grandpa: GrandpaId,
	babe: BabeId,
	im_online: ImOnlineId,
	authority_discovery: AuthorityDiscoveryId,
	sunl: SunLeadId,
) -> serde_json::Value {
	serde_json::json!({
		"authority_discovery": authority_discovery,
		"babe": babe,
		"grandpa": grandpa,
		"im_online": im_online,
		"sunl": sunl,
	})
}

/// Extract some accounts from endowed to be put into the collective.
fn collective(endowed: &[AccountId]) -> Vec<AccountId> {
	const MAX_COLLECTIVE_SIZE: usize = 50;
	let endowed_accounts_count = endowed.len();
	endowed
		.iter()
		.take((endowed_accounts_count.div_ceil(2)).min(MAX_COLLECTIVE_SIZE))
		.cloned()
		.collect()
}

/// Helper function to create RuntimeGenesisConfig json patch for sun cred.
pub fn sunchain_genesis_patch(
	initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		SunLeadId,
	)>,
	initial_nominators: Vec<AccountId>,
	root_key: AccountId,
	endowed_accounts: Vec<AccountId>,
	// Accounts allowed to publish index prices. Kept a parameter rather than a constant
	// because they hold this chain's feed keys: a chain that reused another chain's keys
	// here would be signing prices for an account it does not control.
	oracle_sources: Vec<AccountId>,
) -> serde_json::Value {
	let mut stash = HashMap::new();
	for (idx,var) in initial_authorities.iter().enumerate() {
		let divisor = 1u128.pow(idx as u32);
		let s = STASH.checked_div(divisor).unwrap_or(0);
		stash.insert(var.0.clone(), s);
	}
	let (initial_authorities, endowed_accounts, stakers) =
		configure_accounts(initial_authorities, initial_nominators, endowed_accounts, stash);

	let validator_count = initial_authorities.len();
	let minimum_validator_count = validator_count;

	let collective = collective(&endowed_accounts);

	let mut genesis = serde_json::json!({
		"balances": {
			"balances": endowed_accounts.iter().cloned().map(|x| (x, ENDOWMENT)).collect::<Vec<_>>()
		},
		"session": {
			"keys": initial_authorities
			.iter()
			.map(|x| {
				(
					x.0.clone(),
					// stash account is controller
					x.0.clone(),
							session_keys_json(
						x.2.clone(),
						x.3.clone(),
						x.4.clone(),
						x.5.clone(),
						x.6.clone(),
						)
					// x.0.clone(), x.1.clone(), x.2.clone()
				)
			})
			.collect::<Vec<_>>()
		},
		// "elections": {
		// 	"members": collective.iter().cloned().map(|member| (member, STASH)).collect::<Vec<_>>(),
		// },
		// "technicalCommittee": {
		// 	"members": collective,
		// },
		"staking": {
			"validatorCount": validator_count + 50,
			"minimumValidatorCount": minimum_validator_count,
			"invulnerables": initial_authorities
				.iter()
				.map(|x| x.0.clone())
				.collect::<Vec<_>>(),
			"stakers": stakers,
			"slashRewardFraction": Perbill::from_percent(10),
		},
		"sudo": {
			"key": root_key,
		},
		"babe": {
			// "authorities": initial_authorities
			// 	.iter()
			// 	.map(|x| (x.2.clone(), x.3.clone()))
			// 	.collect::<Vec<_>>(),
			"epochConfig": BABE_GENESIS_EPOCH_CONFIG,
		},
		// Activate SunLead as the authoring consensus at genesis so the node's startup
		// gate (which reads `is_active` at block 0) picks the SunLead import queue +
		// authoring path. Without this the flag defaults to `false` and BABE is used.
		"sunLead": {
			"sunleadActive": true,
		},
		"evmChainId": { "chainId": 42u32 }
	});
	crate::chain_spec::dex_genesis::merge_dex_genesis(&mut genesis, oracle_sources, vec![0, 1, 2, 3]);

	// `deposit_collateral` moves real USDC into custody. Without the asset in genesis the
	// DEX cannot be funded at all: every deposit fails with `AssetTransferFailed`, and the
	// reason is invisible from the outside.
	let usdc = crate::chain_spec::test_usdc_genesis_json(root_key.clone(), endowed_accounts.clone());
	for (key, value) in usdc.as_object().expect("USDC genesis is a JSON object") {
		genesis.as_object_mut().expect("genesis is a JSON object").insert(key.clone(), value.clone());
	}
	genesis
}

pub fn staging_sunchain_genesis_patch() -> serde_json::Value {
	let (initial_authorities, root_key, endowed_accounts) =
		configure_accounts_for_staging_sunchain();
	// The keys this node's feed signs with, resolved from the same place the feed reads them
	// (`crate::oracle_keys`). Two, because the oracle needs `MinSources` to agree before a
	// price exists at all.
	let oracle_sources = crate::oracle_keys::source_accounts();
	let v = sunchain_genesis_patch(
		initial_authorities,
		vec![],
		root_key,
		endowed_accounts,
		oracle_sources,
	);
	warn!(target: "sunchain", "sunchain_genesis_patch : {:?}", v);
	v
}

/// Helper function to generate stash, controller and session key from seed.
///
/// Note: `//` is prepended internally.
// pub fn authority_keys_from_seed(seed: &str) -> (AccountId, AccountId, SessionKeys) {
// 	let auth_pair = core_sr25519::Pair::from_string_with_seed(
// 		&format!("0x{seed}"),
// 		None,
// 	).unwrap().0;
// 	(
// 		auth_pair.public().into(),
// 		auth_pair.public().into(),
// 		// get_public_from_string_or_panic::<core_sr25519::Public>(&format!("{seed}//stash")).into(),
// 		// get_public_from_string_or_panic::<core_sr25519::Public>(seed).into(),
// 		session_keys_from_seed(seed),
// 	)
// }

pub fn session_keys(
	grandpa: GrandpaId,
	babe: BabeId,
	im_online: ImOnlineId,
	authority_discovery: AuthorityDiscoveryId,
	sunl: SunLeadId,
) -> SessionKeys {
	SessionKeys { grandpa, babe, im_online, authority_discovery, sunl }
}

/// We have this method as there is no straight forward way to convert the
/// account keyring into these ids.
///
/// Note: `//` is prepended internally.
pub fn session_keys_from_seed(seed: &str) -> SessionKeys {
	let sr_auth_pair = core_sr25519::Pair::from_string_with_seed(
		&format!("0x{seed}"),
		None,
	).unwrap().0;
	let ed_auth_pair = ed25519::Pair::from_string_with_seed(
		&format!("0x{seed}"),
		None,
	).unwrap().0;
	session_keys(
		// get_public_from_string_or_panic::<GrandpaId>(seed),
		// get_public_from_string_or_panic::<BabeId>(seed),
		// get_public_from_string_or_panic::<ImOnlineId>(seed),
		// get_public_from_string_or_panic::<AuthorityDiscoveryId>(seed),
		ed_auth_pair.public().into(),
		sr_auth_pair.public().into(),
		sr_auth_pair.public().into(),
		sr_auth_pair.public().into(),
		sr_auth_pair.public().into(),
	)
}
