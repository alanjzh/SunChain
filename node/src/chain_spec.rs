use sc_service::{ChainType, Properties};
use sunchain_runtime::WASM_BINARY;
use ecdsa_keyring::Keyring;

use crate::chain_spec::initial_config::staging_sunchain_genesis_patch;

pub mod initial_config;
pub mod sunchain_spec;
pub mod dex_genesis;
use sunchain_runtime::BABE_GENESIS_EPOCH_CONFIG;
use sp_runtime::Perbill;
pub use sunchain_runtime::{
	Block, AccountId, Balance, UNIT,
	configs::{
		COLLATERAL_ASSET_DECIMALS, COLLATERAL_ASSET_ID, COLLATERAL_ASSET_MIN_BALANCE, SessionKeys,
	},
};
pub const ENDOWMENT: Balance = 1_000_000 * UNIT;

/// Genesis endowment of the Phase 1 test USDC, in asset atomic units (6 decimals).
pub const TEST_USDC_ENDOWMENT: u128 = 1_000_000_000_000u128; // 1,000,000 USDC
use sp_consensus_babe::AuthorityId as BabeId;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_grandpa::AuthorityId as GrandpaId;
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;
use sunlead_primitives::SunLeadId;
use sp_core::{crypto::UncheckedInto, ecdsa, sr25519, Pair, Public, H160, U256};
use sp_staking::{
	StakerStatus,
};
use serde::{Deserialize, Serialize};
use sc_chain_spec::ChainSpecExtension;


/// Specialized `ChainSpec`. This is a specialization of the general Substrate ChainSpec type.
/// /// Node `ChainSpec` extensions.
///
/// Additional parameters for some Substrate core modules,
/// customizable from the chain spec.
#[derive(Default, Clone, Serialize, Deserialize, ChainSpecExtension)]
#[serde(rename_all = "camelCase")]
pub struct Extensions {
    /// Block numbers with known hashes.
    pub fork_blocks: sc_client_api::ForkBlocks<Block>,
    /// Known bad block hashes.
    pub bad_blocks: sc_client_api::BadBlocks<Block>,
    /// The light sync state extension used by the sync-state rpc.
    pub light_sync_state: sc_sync_state_rpc::LightSyncStateExtension,
}
pub type ChainSpec = sc_service::GenericChainSpec<Extensions>;

fn session_keys(
    // ed25519
    grandpa: GrandpaId,
    // sr25519
    babe: BabeId,
    // sr25519
    im_online: ImOnlineId,
    // sr25519
    authority_discovery: AuthorityDiscoveryId,
    // sr25519 (SunLead deterministic-leader session key)
    sunl: SunLeadId,
) -> SessionKeys {
    SessionKeys {
        grandpa,
        babe,
        im_online,
        authority_discovery,
        sunl,
    }
}


/// The accounts that receive a genesis balance in the dev/local chain specs.
fn default_endowed_accounts() -> Vec<AccountId> {
    vec![
        Keyring::Alith.into(),
        Keyring::Baltathar.into(),
        Keyring::CharLeth.into(),
        Keyring::Dorothy.into(),
        Keyring::Ethan.into(),
        Keyring::Faith.into(),
    ]
}

/// Genesis section that creates the Phase 1 test USDC and endows `holders` with it.
///
/// `is_sufficient = true` is required: the exchange custody account is a bare `PalletId`
/// account holding no native balance, so a non-sufficient asset could never be created for
/// it (design §4.3). The asset is owned by `owner`, which may mint or burn further units.
pub fn test_usdc_genesis_json(owner: AccountId, holders: Vec<AccountId>) -> serde_json::Value {
    serde_json::json!({
        "assets": {
            "assets": vec![(
                COLLATERAL_ASSET_ID,
                owner,
                true,
                COLLATERAL_ASSET_MIN_BALANCE,
            )],
            "metadata": vec![(
                COLLATERAL_ASSET_ID,
                b"Test USDC".to_vec(),
                b"USDC".to_vec(),
                COLLATERAL_ASSET_DECIMALS,
            )],
            "accounts": holders
                .into_iter()
                .map(|x| (COLLATERAL_ASSET_ID, x, TEST_USDC_ENDOWMENT))
                .collect::<Vec<_>>(),
        },
    })
}

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
    endowed_accounts: Option<Vec<AccountId>>,
    stash: Balance,
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
    usize,
    Vec<(AccountId, AccountId, Balance, StakerStatus<AccountId>)>,
) {
    let mut endowed_accounts: Vec<AccountId> = endowed_accounts.unwrap_or_else(default_endowed_accounts);
    // endow all authorities and nominators.
    initial_authorities
        .iter()
        .map(|x| &x.0)
        .chain(initial_nominators.iter())
        .for_each(|x| {
            if !endowed_accounts.contains(x) {
                endowed_accounts.push(x.clone())
            }
        });

    // stakers: all validators and nominators.
    let stakers = initial_authorities
        .iter()
        .map(|x| (x.0.clone(), x.0.clone(), stash, StakerStatus::Validator))
        .collect::<Vec<_>>();

    let num_endowed_accounts = endowed_accounts.len();

    (
        initial_authorities,
        endowed_accounts,
        num_endowed_accounts,
        stakers,
    )
}

/// Helper function to create RuntimeGenesisConfig json patch for testing.
pub fn testnet_genesis(
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
    endowed_accounts: Option<Vec<AccountId>>,
    extra_endowed_accounts_balance: Vec<(AccountId, u128)>,
    evm_chain_id: u32,
) -> serde_json::Value {
    let (initial_authorities, endowed_accounts, num_endowed_accounts, stakers) = configure_accounts(
        initial_authorities,
        initial_nominators,
        endowed_accounts,
        10 * UNIT,
    );

    let mut genesis = serde_json::json!({
        "balances": {
            "balances": endowed_accounts.iter().cloned().map(|x| (x, ENDOWMENT)).chain(extra_endowed_accounts_balance).collect::<Vec<_>>(),
        },
        "session": {
            "keys": initial_authorities
                .iter()
                .map(|x| {
                    (
                        x.0.clone(),
                        x.0.clone(),
                        session_keys(
                            x.2.clone(),
                            x.3.clone(),
                            x.4.clone(),
                            x.5.clone(),
                            x.6.clone(),
                        ),
                    )
                })
                .collect::<Vec<_>>(),
        },

        "staking": {
            "validatorCount": initial_authorities.len() + 50,
            "minimumValidatorCount": initial_authorities.len() as u32,
            "invulnerables": initial_authorities.iter().map(|x| x.0.clone()).collect::<Vec<_>>(),
            "slashRewardFraction": Perbill::from_percent(10),
            "stakers": stakers.clone(),
        },
        "sudo": { "key": Some(root_key.clone()) },
        "babe": {
            "epochConfig": BABE_GENESIS_EPOCH_CONFIG,
        },
        // Phase 0: activate SunLead as the authoring consensus at genesis so the node's
        // startup gate (which reads `is_active` at block 0) picks the SunLead import
        // queue + authoring path. Set to `false` (or omit) to keep BABE.
        "sunLead": {
            "sunLeadActive": true,
        },
        // "nominationPools": {
        //     "minCreateBond": 10 * UNIT,
        //     "minJoinBond": 1 * UNIT,
        // },
    });

    // P2-A/P2-D: the DEX half of genesis — tiers, markets, and the oracle that prices
    // them — is shared with the `sunchain` spec, so the two cannot drift apart. The oracle
    // sources come from `oracle_keys` for the same reason: they have to be the keys the feed
    // signs with, or prices silently never exist.
    dex_genesis::merge_dex_genesis(
        &mut genesis,
        crate::oracle_keys::source_accounts(),
        vec![0, 1, 2, 3],
    );

    // Phase 1 test USDC: created at genesis so the exchange core has a collateral asset to
    // work with (design §4.3 / §16).
    let usdc = test_usdc_genesis_json(root_key.clone(), endowed_accounts.clone());
    let genesis_object = genesis.as_object_mut().expect("genesis is a JSON object");
    for (key, value) in usdc.as_object().expect("USDC genesis is a JSON object") {
        genesis_object.insert(key.clone(), value.clone());
    }
    genesis
}


/// Helper function to generate a crypto pair from seed.
pub fn get_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
    TPublic::Pair::from_string(&format!("//{}", seed), None)
        .expect("static values are valid; qed")
        .public()
}

/// Helper function to generate stash, controller and session key from seed.
pub fn authority_keys_from_alice() -> (
    AccountId,
    AccountId,
    GrandpaId,
    BabeId,
    ImOnlineId,
    AuthorityDiscoveryId,
    SunLeadId,
) {
    let seed = "Alice";
    (
        Keyring::Alith.into(),
        Keyring::Alith.into(),
		// sp_keyring::Sr25519Keyring::Alice.public().into(),
        get_from_seed::<GrandpaId>(seed),
		// sp_keyring::Ed25519Keyring::Alice.public().into(),
        get_from_seed::<BabeId>(seed),
        get_from_seed::<ImOnlineId>(seed),
        get_from_seed::<AuthorityDiscoveryId>(seed),
        get_from_seed::<SunLeadId>(seed),
    )
}

pub fn authority_keys_from_bob() -> (
    AccountId,
    AccountId,
    GrandpaId,
    BabeId,
    ImOnlineId,
    AuthorityDiscoveryId,
    SunLeadId,
) {
    let seed = "Bob";
    (
        Keyring::Baltathar.into(),
        Keyring::Baltathar.into(),
        get_from_seed::<GrandpaId>(seed),
        get_from_seed::<BabeId>(seed),
        get_from_seed::<ImOnlineId>(seed),
        get_from_seed::<AuthorityDiscoveryId>(seed),
        get_from_seed::<SunLeadId>(seed),
    )
}



fn development_config_genesis_json() -> serde_json::Value {
    let extra_endowed_accounts_balance = vec![
		// (Keyring::Alith.into(), 100_000_000_000_000 * UNIT),
		(Keyring::Baltathar.into(), 100_000_000 * UNIT)
		];
    testnet_genesis(
        vec![authority_keys_from_alice(), authority_keys_from_bob()],
        vec![],
        Keyring::Alith.into(),
        Some(vec![Keyring::Alith.into(), Keyring::Baltathar.into()]),
        extra_endowed_accounts_balance,
        42u32,
    )
}

pub fn development_chain_spec() -> Result<ChainSpec, String> {
	let mut properties = Properties::new();
	properties.insert("tokenSymbol".into(), "SUN".into());
    properties.insert("tokenDecimals".into(), 18.into());
    properties.insert("isEthereum".into(), true.into());


	Ok(ChainSpec::builder(
		WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?,
		Default::default(),
	)
	.with_name("SunChainDev")
	.with_id("SUN")
	.with_chain_type(ChainType::Development)
	.with_properties(properties)	
	// .with_genesis_config_preset_name(sp_genesis_builder::DEV_RUNTIME_PRESET)
	.with_genesis_config_patch(development_config_genesis_json())
	.build())
}

pub fn local_chain_spec() -> Result<ChainSpec, String> {
	Ok(ChainSpec::builder(
		WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?,
		Default::default(),
	)
	.with_name("Local Testnet")
	.with_id("local_testnet")
	.with_chain_type(ChainType::Local)
	.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
	// The runtime preset does not create the Phase 1 test USDC; patch it in (the patch is
	// merged onto the preset) so `local` has the same collateral asset as `dev`.
	.with_genesis_config_patch(test_usdc_genesis_json(
		Keyring::Alith.into(),
		default_endowed_accounts(),
	))
	.build())
}
