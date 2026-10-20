use sc_service::{ChainType, Properties};
use sc_chain_spec::ChainSpecExtension;
use sunchain_runtime::WASM_BINARY;

use sunchain_runtime::{
    Block, 
    // genesis_config_presets::SUNCHAIN_RUNTIME_PRESET,
};
use super::initial_config::*;
use serde::{Deserialize, Serialize};
// The URL for the telemetry server.
// const TELEMETRY_URL: &str = "wss://telemetry.polkadot.io/submit/";

/// Node `ChainSpec` extensions.
///
/// Additional parameters for some Suncahin core modules,
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

pub fn sun_chain_spec() -> Result<ChainSpec, String> {
    let mut properties = Properties::new();
    properties.insert("tokenSymbol".into(), "SUN".into());
    properties.insert("tokenDecimals".into(), 18.into());
    properties.insert("isEthereum".into(), true.into());

    let wasm_binary = WASM_BINARY.ok_or_else(|| "Mainnet wasm not available".to_string())?;

    Ok(ChainSpec::builder(wasm_binary, Default::default())
        .with_name("SunChain")
        .with_id("SUN")
        .with_chain_type(ChainType::Live)
        .with_properties(properties)
        // .with_boot_nodes(boot_nodes)
        // .with_protocol_id("substrate")//e.g : BareSr25519
        // .with_genesis_config_preset_name(SUNCHAIN_RUNTIME_PRESET)
        .with_genesis_config_patch(staging_sunchain_genesis_patch())
        .build())
}

