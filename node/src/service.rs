//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use sunchain_runtime::configs::TransactionConverter;
use fc_consensus::FrontierBlockImport;
use fc_rpc::pending;
use fc_rpc::EthConfig;
use futures::prelude::*;
use futures::FutureExt;
use sc_client_api::{Backend, BlockBackend};
use crate::oracle_feed::{FeedConfig, FeedMarket};
use crate::pricefeed::{ConversionDetails, Exchange};
use sc_consensus_aura::{ImportQueueParams, SlotProportion, StartAuraParams};
use sc_consensus_babe::{self, SlotProportion as BabeSlotProportion};
use sc_consensus_grandpa::SharedVoterState;
use sc_network::{
    event::Event, service::traits::NetworkService, NetworkBackend, NetworkEventStream,
};
use sc_network_sync::service::network;
use sc_network_sync::{strategy::warp::WarpSyncConfig, SyncingService};
use sc_service::{error::Error as ServiceError, Configuration, RpcHandlers, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sc_transaction_pool::TransactionPoolHandle;
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_tracing::tracing_subscriber::filter;
use sp_core::U256;
use crate::cli::BackendType;
use crate::cli::Cli;
use crate::cli::EthConfiguration;
use crate::eth::db_config_dir;
use crate::eth::new_frontier_partial;
use crate::eth::spawn_frontier_tasks;
use crate::eth::FrontierBackend;
use crate::eth::FrontierPartialComponents;
// use polkadot_sdk::{
//     sc_consensus_beefy as beefy, sc_consensus_beefy::BeefyRPCLinks,
//     sc_consensus_grandpa as grandpa, sp_consensus_beefy as beefy_primitives, *,
// };
use crate::rpc as node_rpc;
use sunchain_runtime::{self, apis::RuntimeApi, opaque::Block};
use sc_consensus_babe::{BabeWorkerHandle};
use sp_tracing::info;
use std::f32::consts::E;
use std::{sync::Arc, time::Duration};
use babe_consensus_data_provider::BabeConsensusDataProvider;
use frame_benchmarking_cli::SUBSTRATE_REFERENCE_HARDWARE;

use crate::sunlead_consensus::{
	start_sunlead, sunlead_import_queue, SunLeadBlockImport, SunLeadParams,
};
use sp_api::ProvideRuntimeApi;
use sp_core::crypto::KeyTypeId;
use sunchain_runtime::apis::SunLeadApi;
use sunlead_primitives::SUNLEAD_ENGINE_ID;

/// Ensure the well-known dev session keys (babe/grandpa/imon/audi/sunl) for `seed`
/// (e.g. `//Alice`) are present in the keystore.
///
/// Substrate only seeds the keystore from `config.dev_key_seed` inside
/// `sc_service::spawn_tasks` via `sp_session::generate_initial_session_keys`. When
/// that field is empty (e.g. a `--chain dev` node started without
/// `--alice`/`--bob`/`--dev`, or any dev plumbing gap) the keys are never
/// written — leaving the node without the `sunl` key it needs to author SunLead
/// blocks and without the `gran` key GRANDPA needs to vote, so the chain authors
/// blocks but never finalizes. We inject them explicitly at startup so a dev node
/// is always ready to author and finalize, regardless of how the dev seed was (or
/// wasn't) plumbed through `Configuration`.
fn inject_dev_session_keys(keystore: &sp_keystore::KeystorePtr, seed: &str) {
	let _ = keystore.sr25519_generate_new(KeyTypeId(*b"babe"), Some(seed));
	let _ = keystore.ed25519_generate_new(KeyTypeId(*b"gran"), Some(seed));
	let _ = keystore.sr25519_generate_new(KeyTypeId(*b"imon"), Some(seed));
	let _ = keystore.sr25519_generate_new(KeyTypeId(*b"audi"), Some(seed));
	let _ = keystore.sr25519_generate_new(sunlead_primitives::KEY_TYPE, Some(seed));
	log::info!(target: "sunchain", "injected dev session keys (seed {}) into keystore", seed);
}

use fc_storage::StorageOverrideHandler;
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, NumberFor};
use std::path::Path;
//use beefy_primitives::ecdsa_crypto::Public;
/// Host functions required for kitchensink runtime and Substrate node.
#[cfg(not(feature = "runtime-benchmarks"))]
pub type HostFunctions = (
    sp_io::SubstrateHostFunctions,
    sp_statement_store::runtime_api::HostFunctions,
	cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
);

/// Host functions required for kitchensink runtime and Substrate node.
#[cfg(feature = "runtime-benchmarks")]
pub type HostFunctions = (
    sp_io::SubstrateHostFunctions,
    sp_statement_store::runtime_api::HostFunctions,
    cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
);
/// Upper bound on actions packed into one exchange batch; must not exceed the runtime's
/// `MaxActionsPerBatch`, which the extrinsic re-checks anyway. Defined in the proposer, which
/// is the only other user.
pub(crate) use crate::exchange_proposer::MAX_EXCHANGE_BATCH_ACTIONS;

pub(crate) type FullClient = sc_service::TFullClient<
    Block,
    RuntimeApi,
    sc_executor::WasmExecutor<HostFunctions>,
>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;
type FullGrandpaBlockImport =
    sc_consensus_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>;

/// The minimum period of blocks on which justifications will be
/// imported and generated.
const GRANDPA_JUSTIFICATION_PERIOD: u32 = 512;

// pub type Service = sc_service::PartialComponents<
// 	FullClient,
// 	FullBackend,
// 	FullSelectChain,
// 	sc_consensus::DefaultImportQueue<Block>,
// 	sc_transaction_pool::TransactionPoolHandle<Block, FullClient>,
// 	(
// 		impl Fn(
// 			sc_rpc::SubscriptionTaskExecutor,
// 		) -> Result<jsonrpsee::RpcModule<()>, sc_service::Error>,
// 		// sc_consensus_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>,
// 		// sc_consensus_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
// 		(
// 			sc_consensus_babe::BabeBlockImport<
// 				Block,
// 				FullClient,
// 				FullGrandpaBlockImport,
// 			>,
// 			sc_consensus_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
// 			sc_consensus_babe::BabeLink<Block>,
// 		),
// 		sc_consensus_grandpa::SharedVoterState,
// 		Option<Telemetry>,
// 	),
// >;



type FullGrandpa = sc_consensus_grandpa::GrandpaBlockImport<
	FullBackend,
	Block,
	FullClient,
	FullSelectChain,
>;
type FullBabe = sc_consensus_babe::BabeBlockImport<
	Block,
	FullClient,
	FullGrandpa,
>;
type FullFrontier = FrontierBlockImport<Block, FullBabe,FullClient>;
type EffectiveFullBlockImport = FullFrontier;

// The effective block import used by SunLead: it applies the SunLead pre-digest
// then delegates to the Frontier -> GRANDPA import chain (BABE is bypassed).
type SunLeadEffectiveImport = FrontierBlockImport<Block, FullGrandpa, FullClient>;
type FullSunLeadBlockImport = SunLeadBlockImport<Block, SunLeadEffectiveImport>;

pub fn new_partial<NB>(
    config: &Configuration,
    eth_config: &EthConfiguration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block>,
        sc_transaction_pool::TransactionPoolHandle<Block, FullClient>,
        (

            // sc_consensus_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>,
            // sc_consensus_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
            (
				Option<
					sc_consensus_babe::BabeBlockImport<
						Block,
						FullClient,
						FrontierBlockImport<Block, FullGrandpaBlockImport, FullClient>,
					>,
				>,
                sc_consensus_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
                Option<sc_consensus_babe::BabeLink<Block>>,
            ),
            Option<Telemetry>,
			Option<BabeWorkerHandle<Block>>,
            bool,
            FullSunLeadBlockImport,
        ),
    >,
    ServiceError,
>
where
    NB: sc_network::NetworkBackend<Block, <Block as BlockT>::Hash>,
    // B: BlockT<Hash = H256>,
{
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = sc_service::new_wasm_executor::<HostFunctions>(
        &config.executor,
    );
    let (client, backend, keystore_container, task_manager) = sc_service::new_full_parts(
        config,
        telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
        executor,
    )?;
    let client = Arc::new(client);

    // --- Dev session-key injection (root-cause 2 fix / decision A) ---
    // Make sure the dev node's keystore holds the session keys the genesis
    // registered (babe/grandpa/imon/audi/sunl), so it can both author SunLead
    // blocks (sunl) and vote in GRANDPA (gran). `spawn_tasks` only does this when
    // `config.dev_key_seed` is set; we cover the gap (and dev chains started
    // without `--alice`/`--bob`/`--dev`) and inject only the local node's seed to
    // avoid cross-node equivocation.
    {
        let dev_seed = config
            .dev_key_seed
            .clone()
            .or_else(|| {
                use sc_chain_spec::ChainSpec as _;
                (config.chain_spec.chain_type() == sc_chain_spec::ChainType::Development)
                    .then(|| "//Alice".to_string())
            });
        if let Some(seed) = dev_seed {
            inject_dev_session_keys(&keystore_container.keystore(), &seed);
        }
    }

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager
            .spawn_handle()
            .spawn("telemetry", None, worker.run());
        telemetry
    });

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

	// move this to a separate function


    let transaction_pool = Arc::from(
        sc_transaction_pool::Builder::new(
            task_manager.spawn_essential_handle(),
            client.clone(),
            config.role.is_authority().into(),
        )
        .with_options(config.transaction_pool.clone())
        .with_prometheus(config.prometheus_registry())
        .build(),
    );

    let (grandpa_block_import, grandpa_link) = sc_consensus_grandpa::block_import(
        client.clone(),
        GRANDPA_JUSTIFICATION_PERIOD,
        &(client.clone() as Arc<_>), //&client,
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;


	
	let frontier_block_import = FrontierBlockImport::new(
        grandpa_block_import.clone(),
        client.clone(),
    );

	// BABE's `block_import` eagerly finalizes the current finalized head on
	// construction, which panics when that head is a SunLead block (it carries no
	// BABE pre-digest). We therefore only build it when BABE is actually the
	// active authoring consensus; SunLead chains never construct it. The slot
	// duration is read from the runtime config (no construction) and is needed by
	// both consensus paths for the timestamp<->slot mapping.
	let babe_config = sc_consensus_babe::configuration(&*client)?;
	let slot_duration = babe_config.slot_duration();

	let justification_import = grandpa_block_import.clone();

    // let cidp_client = client.clone();
    // let import_queue =
    // 	sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _>(ImportQueueParams {
    // 		block_import: grandpa_block_import.clone(),
    // 		justification_import: Some(Box::new(grandpa_block_import.clone())),
    // 		client: client.clone(),
    // 		create_inherent_data_providers: move |parent_hash, _| {
    // 			let cidp_client = cidp_client.clone();
    // 			async move {
    // 				let slot_duration = sc_consensus_aura::standalone::slot_duration_at(
    // 					&*cidp_client,
    // 					parent_hash,
    // 				)?;
    // 				let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

    // 				let slot =
    // 					sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
    // 						*timestamp,
    // 						slot_duration,
    // 					);

    // 				Ok((slot, timestamp))
    // 			}
    // 		},
    // 		spawner: &task_manager.spawn_essential_handle(),
    // 		registry: config.prometheus_registry(),
    // 		check_for_equivocation: Default::default(),
    // 		telemetry: telemetry.as_ref().map(|x| x.handle()),
    // 		compatibility_mode: Default::default(),
    // 	})?;

    // --- SunLead parallel consensus (gated, BABE remains the default) ---
    // Only build the import queue that is actually used. Constructing BOTH unconditionally
    // spawns a `basic-block-import-worker` essential task for the unused one, which is then
    // dropped and tears the whole service down at genesis.
    let sunlead_block_import = SunLeadBlockImport::new(frontier_block_import.clone());
    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");
    let sunlead_api = sp_api::ProvideRuntimeApi::runtime_api(client.as_ref());
    // Decide which authoring consensus is active. `SunLeadActive` is a genesis-set
    // constant, so its value is identical at every block — but the *genesis* state
    // can be pruned from the DB on a long-running restart, which makes a runtime-API
    // call at `block_hash(0)` fail with `UnknownBlock("State already discarded")`.
    // Reading it at the (recent, unpruned) finalized head avoids that; as a last
    // resort we fall back to inspecting the finalized head's digest for the SunLead
    // engine id so a validator reboot can never be misclassified as BABE.
    let finalized_hash = client.chain_info().finalized_hash;
    let use_sunlead = match sunchain_runtime::apis::SunLeadApi::is_active(&*sunlead_api, finalized_hash) {
        Ok(active) => active,
        Err(e) => {
            log::warn!(
                target: "sunlead",
                "is_active API at finalized head failed ({:?}); falling back to digest inspection",
                e
            );
            client
                .header(finalized_hash)
                .ok()
                .flatten()
                .map(|h: <Block as sp_runtime::traits::Block>::Header| {
                    sp_runtime::traits::Header::digest(&h)
                        .logs()
                        .iter()
                        .any(|l| l.as_pre_runtime().map(|(id, _)| id) == Some(SUNLEAD_ENGINE_ID))
                })
                .unwrap_or(false)
        }
    };
    log::info!(target: "sunlead", "SunLead activation flag (is_active) = {}", use_sunlead);

    // Only construct BABE's block import / link when BABE is the active authoring
    // consensus. On a SunLead chain this must stay `None`: building it eagerly
    // finalizes the current finalized head, which (being a SunLead block) carries
    // no BABE pre-digest and would panic the node at startup (a validator reboot
    // would then bring the chain down). `slot_duration` above already came from the
    // runtime config, so SunLead's timestamp<->slot mapping is unaffected.
    let (babe_block_import, babe_link) = if use_sunlead {
    	(None, None)
    } else {
    	let (bi, bl) = sc_consensus_babe::block_import(
    		babe_config,
    		frontier_block_import.clone(),
    		client.clone(),
    	)?;
    	(Some(bi), Some(bl))
    };

    let (import_queue, babe_worker_handle): (
        sc_consensus::import_queue::BasicQueue<Block>,
        Option<BabeWorkerHandle<Block>>,
    ) = if use_sunlead {
        let iq = sunlead_import_queue(
            sunlead_block_import.clone(),
            client.clone(),
            Some(Box::new(justification_import.clone())),
            genesis_hash,
            &task_manager.spawn_essential_handle(),
            config.prometheus_registry(),
        );
        (iq, None)
    } else {
        let (iq, bh) = sc_consensus_babe::import_queue(sc_consensus_babe::ImportQueueParams {
            link: babe_link
                .clone()
                .expect("BABE link present when BABE is the active consensus"),
            block_import: babe_block_import
                .clone()
                .expect("BABE block import present when BABE is the active consensus"),
            justification_import: Some(Box::new(justification_import.clone())),
            client: client.clone(),
            select_chain: select_chain.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
					sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
						*timestamp,
						slot_duration,
					);

                Ok((slot, timestamp))
            },
            spawner: &task_manager.spawn_essential_handle(),
            registry: config.prometheus_registry(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool.clone()),
        })?;
        (iq, Some(bh))
    };

    // let PartialComponents {
    // 	client,
    // 	backend,
    // 	mut task_manager,
    // 	import_queue,
    // 	keystore_container,
    // 	select_chain,
    // 	transaction_pool,
    // 	other: (mut telemetry, block_import, grandpa_link, frontier_backend, storage_override),
    // } = new_partial(&config, &eth_config, import_queue)?;

    // let FrontierPartialComponents {
    // 	filter_pool,
    // 	fee_history_cache,
    // 	fee_history_cache_limit,
    // } = new_frontier_partial(&eth_config)?;



    // let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
    //     backend.clone(),
    //     grandpa_link.shared_authority_set().clone(),
    //     Vec::default(),
    // ));

    // let (network, system_rpc_tx, tx_handler_controller, sync_service) =
    //     sc_service::build_network(sc_service::BuildNetworkParams {
    //         config: &config,
    //         net_config,
    //         client: client.clone(),
    //         transaction_pool: transaction_pool.clone(),
    //         spawn_handle: task_manager.spawn_handle(),
    //         import_queue: import_queue,
    //         block_announce_validator_builder: None,
    //         warp_sync_config: Some(WarpSyncConfig::WithProvider((warp_sync))),
    //         block_relay: None,
    //         metrics,
    //     })?;


     let import_setup = (babe_block_import, grandpa_link, babe_link);

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (import_setup, telemetry, babe_worker_handle, use_sunlead, sunlead_block_import),
    })
}

/// Result of [`new_full_base`].
pub struct NewFullBase {
    /// The task manager of the node.
    pub task_manager: TaskManager,
    /// The client instance of the node.
    pub client: Arc<FullClient>,
    /// The networking service of the node.
    pub network: Arc<dyn NetworkService>,
    /// The syncing service of the node.
    pub sync: Arc<SyncingService<Block>>,
    /// The transaction pool of the node.
    pub transaction_pool: Arc<TransactionPoolHandle<Block, FullClient>>,
    /// The rpc handlers of the node.
    pub rpc_handlers: RpcHandlers,
}

/// Creates a full service from the configuration.
pub async fn new_full_base<N: NetworkBackend<Block, <Block as sp_runtime::traits::Block>::Hash>>(
    config: Configuration,
	eth_config: EthConfiguration,
    with_startup_data: impl FnOnce(
        &Option<
            sc_consensus_babe::BabeBlockImport<
                Block,
                FullClient,
                FrontierBlockImport<Block, FullGrandpaBlockImport, FullClient>,
            >,
        >,
        &Option<sc_consensus_babe::BabeLink<Block>>,
    ),
) -> Result<NewFullBase, ServiceError> {
    let exchange_pool_path = config.base_path.path().join("exchange_pool.bin");
    let exchange_pool = Arc::new(std::sync::Mutex::new(crate::exchange_pool::ExchangePool::new(
        MAX_EXCHANGE_BATCH_ACTIONS,
        Some(exchange_pool_path),
    )));
    let (exchange_outbound, exchange_inbound) = futures::channel::mpsc::unbounded();
    // The RPC closure below is `move`, so it needs its own handle to the same pool.
    let exchange_pool_rpc = exchange_pool.clone();


    let is_offchain_indexing_enabled = config.offchain_worker.indexing_enabled;
    let role = config.role;
    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks =
        Some(sc_consensus_slots::BackoffAuthoringOnFinalizedHeadLagging::default());
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();
    let enable_offchain_worker = config.offchain_worker.enabled;

    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (import_setup, mut telemetry, babe_worker_handle, use_sunlead, sunlead_block_import),
    } = new_partial::<N>(&config, &eth_config)?;

	
    let metrics = N::register_notification_metrics(
        config.prometheus_config.as_ref().map(|cfg| &cfg.registry),
    );
    
	let auth_disc_publish_non_global_ips = config.network.allow_non_globals_in_dht;
    let auth_disc_public_addresses = config.network.public_addresses.clone();

    let mut net_config = sc_network::config::FullNetworkConfiguration::<_, _, N>::new(
        &config.network,
        config
            .prometheus_config
            .as_ref()
            .map(|cfg| cfg.registry.clone()),
    );

    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");
    let peer_store_handle = net_config.peer_store_handle();

    let grandpa_protocol_name =
        sc_consensus_grandpa::protocol_standard_name(&genesis_hash, &config.chain_spec);
    let (grandpa_protocol_config, grandpa_notification_service) =
        sc_consensus_grandpa::grandpa_peers_set_config::<_, N>(
            grandpa_protocol_name.clone(),
            metrics.clone(),
            Arc::clone(&peer_store_handle),
        );
    net_config.add_notification_protocol(grandpa_protocol_config);

    // P2-D §12.1 ②: a dedicated set for gossiping signed exchange actions.
    let exchange_protocol_name: sc_network::ProtocolName =
        crate::exchange_p2p::EXCHANGE_PROTOCOL.into();
    let (exchange_protocol_config, exchange_notification_service) =
        crate::exchange_p2p::exchange_peers_set_config::<_, N>(
            exchange_protocol_name,
            metrics.clone(),
            Arc::clone(&peer_store_handle),
        );
    net_config.add_notification_protocol(exchange_protocol_config);

    let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        import_setup.1.shared_authority_set().clone(),
        Vec::default(),
    ));

    let (network, system_rpc_tx, tx_handler_controller, sync_service) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            net_config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            block_announce_validator_builder: None,
            warp_sync_config: Some(WarpSyncConfig::WithProvider(warp_sync)),
            block_relay: None,
            metrics,
        })?;


    let net_config_path = config.network.net_config_path.clone();

	let storage_override =
       Arc::new(StorageOverrideHandler::<Block, FullClient, FullBackend>::new(client.clone()));

	let FrontierPartialComponents {
		filter_pool,
		fee_history_cache,
		fee_history_cache_limit,
	} = new_frontier_partial(&eth_config)?;

	let filter_pool1 = filter_pool.clone();
	let fee_history_cache1 = fee_history_cache.clone();

	let eth_backend = backend.clone();
	let eth_storage_override = storage_override.clone();

    let (rpc_extensions_builder, rpc_setup,frontier_backend,pubsub_notification_sinks) = {
        let (_, grandpa_link, _) = &import_setup;

        let justification_stream = grandpa_link.justification_stream();
        let shared_authority_set = grandpa_link.shared_authority_set().clone();
        let shared_voter_state = sc_consensus_grandpa::SharedVoterState::empty();
        let shared_voter_state2 = shared_voter_state.clone();

        let finality_proof_provider = sc_consensus_grandpa::FinalityProofProvider::new_for_service(
            backend.clone(),
            Some(shared_authority_set.clone()),
        );

        let client = client.clone();
        let pool = transaction_pool.clone();
        let select_chain = select_chain.clone();
        let keystore = keystore_container.keystore();
        let chain_spec = config.chain_spec.cloned_box();

    // let maybe_registry = config.prometheus_config.as_ref().map(|cfg| &cfg.registry);
    // let metrics = N::register_notification_metrics(maybe_registry);
    // let mut net_config = sc_network::config::FullNetworkConfiguration::<_, _, N>::new(
    //     &config.network,
    //     maybe_registry.cloned(),
    // );
  let frontier_backend = match eth_config.frontier_backend_type {
        BackendType::KeyValue => FrontierBackend::KeyValue(Arc::new(fc_db::kv::Backend::open(
            Arc::clone(&client),
            &config.database,
            &db_config_dir(&config),
        )?)),
        BackendType::Sql => {
            let db_path = db_config_dir(&config).join("sql");
            std::fs::create_dir_all(&db_path).expect("failed creating sql db directory");
            let backend = futures::executor::block_on(fc_db::sql::Backend::new(
                fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
                    path: Path::new("sqlite:///")
                        .join(db_path)
                        .join("frontier.db3")
                        .to_str()
                        .unwrap(),
                    create_if_missing: true,
                    thread_count: eth_config.frontier_sql_backend_thread_count,
                    cache_size: eth_config.frontier_sql_backend_cache_size,
                }),
                eth_config.frontier_sql_backend_pool_size,
                std::num::NonZeroU32::new(eth_config.frontier_sql_backend_num_ops_timeout),
                storage_override.clone(),
            ))
            .unwrap_or_else(|err| panic!("failed creating sql backend: {:?}", err));
            FrontierBackend::Sql(Arc::new(backend))
        }
    };

	let frontier_backend = Arc::new(frontier_backend);
	let frontier_backend1 = frontier_backend.clone();

	let prometheus_registry = config.prometheus_registry().cloned();

    let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
        task_manager.spawn_handle(),
        storage_override.clone(),
        eth_config.eth_log_block_cache,
        eth_config.eth_statuses_cache,
        prometheus_registry.clone(),
    ));

	let pubsub_notification_sinks: fc_mapping_sync::EthereumBlockNotificationSinks<
		fc_mapping_sync::EthereumBlockNotification<Block>,
	> = Default::default();
	let pubsub_notification_sinks1 = Arc::new(pubsub_notification_sinks);
	let pubsub_notification_sinks2 = pubsub_notification_sinks1.clone();
    let rpc_backend = backend.clone();

	// `import_setup.2` (`babe_link`) is `None` on a SunLead chain, so derive the
	// slot duration from the runtime config instead of the BABE link.
	let slot_duration = sc_consensus_babe::configuration(&*client)?.slot_duration();
	let target_gas_price = eth_config.target_gas_price;
    let pending_create_inherent_data_providers = move |_, ()| async move {
            let current = sp_timestamp::InherentDataProvider::from_system_time();
            let next_slot = current.timestamp().as_millis() + slot_duration.as_millis();

            let timestamp = sp_timestamp::InherentDataProvider::new(next_slot.into());
            let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
					*timestamp,
					slot_duration,
				);
            let dynamic_fee = fp_dynamic_fee::InherentDataProvider(U256::from(target_gas_price));
            Ok((slot, timestamp, dynamic_fee))
        };

		let network = network.clone();
        let is_authority = config.role.clone().is_authority().clone();
        let enable_dev_signer = eth_config.enable_dev_signer;
		let sync_service0 = sync_service.clone();
    	let rpc_extensions_builder =
            move | subscription_executor: node_rpc::SubscriptionTaskExecutor| {
				let max_past_logs = eth_config.max_past_logs;
				let execute_gas_limit_multiplier = eth_config.execute_gas_limit_multiplier;
                let eth_deps = node_rpc::EthDeps {
                    client: client.clone(),
                    pool: pool.clone(),
                    graph: pool.clone(),
                    converter: Some(TransactionConverter::<Block>::default()),
                    is_authority: is_authority,
                    enable_dev_signer,
                    network: network.clone(),
                    sync: sync_service0.clone(),
                    frontier_backend: match &*frontier_backend {
                        fc_db::Backend::KeyValue(b) => b.clone(),
                        fc_db::Backend::Sql(b) => b.clone(),
                    },
                    storage_override: storage_override.clone(),
                    block_data_cache: block_data_cache.clone(),
                    filter_pool: filter_pool1.clone(),
                    max_past_logs,
                    fee_history_cache: fee_history_cache1.clone(),
                    fee_history_cache_limit,
                    execute_gas_limit_multiplier,
                    forced_parent_hashes: None,
                    pending_create_inherent_data_providers,
                };

                let deps = node_rpc::FullDeps {
                    client: client.clone(),
                    pool: pool.clone(),
                    select_chain: select_chain.clone(),
                    chain_spec: chain_spec.cloned_box(),
				
					// beefy: node_rpc::BeefyDeps::<beefy_primitives::ecdsa_crypto::AuthorityId> {
                    //     beefy_finality_proof_stream: beefy_rpc_links
                    //         .from_voter_justif_stream
                    //         .clone(),
                    //     beefy_best_block_stream: beefy_rpc_links
                    //         .from_voter_best_beefy_stream
                    //         .clone(),
                    //     subscription_executor: subscription_executor.clone(),
                    // },
                    babe: node_rpc::BabeDeps {
                        keystore: keystore.clone(),
                        babe_worker_handle: babe_worker_handle.clone(),
                    },
                    grandpa: node_rpc::GrandpaDeps {
                        shared_voter_state: shared_voter_state.clone(),
                        shared_authority_set: shared_authority_set.clone(),
                        justification_stream: justification_stream.clone(),
                        subscription_executor: subscription_executor.clone(),
                        finality_provider: finality_proof_provider.clone(),
                    },

                    backend: rpc_backend.clone(),
					//statement_store: rpc_statement_store.clone(),
                    eth: eth_deps,
        exchange_pool: exchange_pool_rpc.clone(),
        exchange_outbound: exchange_outbound.clone(),
                };

                // node_rpc::create_full(deps).map_err(Into::into)
                //node_rpc::create_full(deps).map_err(|e| sc_service::Error::from(e))

				let pending_consenus_data_provider = Box::new(BabeConsensusDataProvider::new(
                    client.clone(),
                    keystore.clone(),
                ));

                node_rpc::create_full(
                    deps,
                    subscription_executor,
                    pubsub_notification_sinks1.clone(),
                    pending_consenus_data_provider,
                )
                .map_err(Into::into)
            };

        (rpc_extensions_builder, shared_voter_state2,frontier_backend1,pubsub_notification_sinks2)
    };

		let shared_voter_state = rpc_setup;

		let network1 = network.clone();


    let rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        config,
        backend: backend.clone(),
        client: client.clone(),
        keystore: keystore_container.keystore(),
        network: network1,
        rpc_builder: Box::new(rpc_extensions_builder),
        transaction_pool: transaction_pool.clone(),
        task_manager: &mut task_manager,
        system_rpc_tx,
        tx_handler_controller,
        sync_service: sync_service.clone(),
        telemetry: telemetry.as_mut(),
    })?;

	spawn_frontier_tasks(
        &task_manager,
        client.clone(),
        eth_backend.clone(),
        frontier_backend,
        filter_pool,
        eth_storage_override.clone(),
        fee_history_cache,
        fee_history_cache_limit,
        sync_service.clone(),
        pubsub_notification_sinks,
    ).await;

    let (block_import, grandpa_link, babe_link) = import_setup;

    (with_startup_data)(&block_import, &babe_link);
    info!(target: "sunchain", "Node role, : {}  enable_grandpa : {}", role, enable_grandpa);
    // P2-D: an action leaves the pool only once the chain has actually consumed it.
    //
    // The proposer never removes anything — it offers the same batch again if it has to.
    // This task re-validates the queue on every imported block and drops what can no longer
    // execute, which is what "already done" looks like from here. Draining at propose time
    // instead would silently lose the action whenever a block was discarded.
    {
        let pool = exchange_pool.clone();
        let client = client.clone();
        let mut notifications =
            sc_client_api::BlockchainEvents::import_notification_stream(client.as_ref());
        task_manager.spawn_handle().spawn("exchange-pool-prune", None, async move {
            use futures::StreamExt;
            log::info!(target: "exchange", "prune task started");

            while let Some(notification) = notifications.next().await {
                let at = notification.hash;
                let actions: Vec<_> = match pool.lock() {
                    Ok(guard) => guard.pending().into_iter().map(|(_, a)| a).collect(),
                    Err(_) => break,
                };
                log::info!(target: "exchange", "prune tick: {} queued", actions.len());
                if actions.is_empty() {
                    continue;
                }

                let api = sp_api::ProvideRuntimeApi::runtime_api(client.as_ref());
                let mut still_valid = std::collections::HashSet::new();
                for action in &actions {
                    let ok = sunchain_runtime::apis::ExchangeRuntimeApi::validate_exchange_action(
                        &*api,
                        at,
                        action.clone(),
                    )
                    .map(|r| r.is_ok())
                    .unwrap_or(false);
                    if ok {
                        still_valid.insert(scale_codec::Encode::encode(action));
                    }
                }

                if let Ok(mut guard) = pool.lock() {
                	let before = guard.len();
                	guard.retain_valid(|a| still_valid.contains(&scale_codec::Encode::encode(a)));
                	let dropped = before.saturating_sub(guard.len());
                	if dropped > 0 {
                		log::info!(target: "exchange", "dropped {dropped} executed action(s)");
                	}
                	// Persist after each prune so a restart reloads the surviving queue (S2-6).
                	if let Err(e) = guard.save() {
                		log::warn!(target: "exchange", "could not persist pool: {e}");
                	}
                }
            }
        });
    }

    // P2-D: premium votes. One vote per block per market, so a sample epoch collects many
    // observations to take the median of, instead of a single one at the boundary.
    {
        let client = client.clone();
        let pool = transaction_pool.clone();
        task_manager.spawn_handle().spawn(
            "premium-votes",
            None,
            crate::premium_feed::run::<Block, _, _, _>(
                client,
                pool,
                crate::premium_feed::PremiumConfig { markets: vec![0, 1, 2, 3] },
            ),
        );
    }

    // P2-A: the price feed daemon. dYdX v4 runs one beside every validator; ours polls the
    // venues and publishes an injected price vote each round (no fee, no balance, no queue).
    {
        let mut markets = Vec::new();
        // Dev-only: the venues and their symbol spellings per market. The tick size is *not*
        // here — the feed reads it from the chain each round, so it cannot disagree with the
        // market it is publishing for.
        //
        // Ids match the dev genesis: 0 BTC, 1 ETH, 2 SOL, 3 DOGE.
        // Binance and OKX quote USDT directly; Kraken's BTC pair is `XBTUSDT` and its DOGE
        // pair is `XDGUSDT`; Bitfinex writes USDT pairs `tBTCUST` and `tDOGE:UST`. All four
        // are USDT-quoted, so no venue needs converting.
        let oracle_markets: Vec<(u32, String, String, String, String)> = vec![
            (0, "BTCUSDT".to_string(), "BTC-USDT".to_string(), "XBTUSDT".to_string(), "tBTCUST".to_string()),
            (1, "ETHUSDT".to_string(), "ETH-USDT".to_string(), "ETHUSDT".to_string(), "tETHUST".to_string()),
            (2, "SOLUSDT".to_string(), "SOL-USDT".to_string(), "SOLUSDT".to_string(), "tSOLUST".to_string()),
            (3, "DOGEUSDT".to_string(), "DOGE-USDT".to_string(), "XDGUSDT".to_string(), "tDOGE:UST".to_string()),
        ];

        for (market_id, symbol_binance, symbol_okx, symbol_kraken, symbol_bitfinex) in oracle_markets {
            markets.push(FeedMarket {
                market_id,
                // Every venue here quotes USDT, so none of them needs converting. A USD
                // venue would carry `ConversionDetails::adjust_by("USD-USDT", 1)`, which
                // multiplies it by how many USDT one dollar buys.
                //
                // Coinbase is deliberately *not* a price source: it has no batch endpoint and
                // answers one market per request, which measured ~2.7 s each behind a proxy —
                // fine for the one peg rate, unusable for hundreds of markets.
                sources: vec![
                    (Exchange::Binance, symbol_binance, ConversionDetails::NONE),
                    (Exchange::Okx, symbol_okx, ConversionDetails::NONE),
                    (Exchange::Kraken, symbol_kraken, ConversionDetails::NONE),
                    (Exchange::Bitfinex, symbol_bitfinex, ConversionDetails::NONE),
                ],
                // The peg, asked for once a round however many markets configure it.
                rate_sources: vec![
                    (Exchange::Okx, "USDT-USD".to_string()),
                    (Exchange::CoinbasePro, "USDT-USD".to_string()),
                ],
                // Four venues back the median; two are enough to publish, so one venue being
                // down (or a market simply not listed on it) does not cost us the price.
                min_exchanges: 2,
                rate_min_exchanges: 1,
                quote_decimals: 6,
            });
        }

        let client = client.clone();
        let pool = transaction_pool.clone();
        task_manager.spawn_handle().spawn(
            "oracle-feed",
            None,
            crate::oracle_feed::run::<Block, _, _, _>(
                client,
                pool,
                FeedConfig {
                    interval: std::time::Duration::from_secs(30),
                    max_age: std::time::Duration::from_secs(60),
                    markets,
                    // Same source of truth as genesis: see `crate::oracle_keys`. Listing the
                    // keys here and the accounts there is how prices used to silently never
                    // exist.
                    signers: crate::oracle_keys::source_keys(),
                },
            ),
        );
    }

    {
        let service = exchange_notification_service;
        let pool = exchange_pool.clone();
        let client = client.clone();
        task_manager.spawn_handle().spawn("exchange-gossip", None, async move {
            crate::exchange_p2p::run_exchange_gossip::<Block, _>(service, pool, client, exchange_inbound).await;
        });
    }

    if let sc_service::config::Role::Authority { .. } = &role {
        // P2-D §12.4: the exchange batch is built by whoever proposes, so it wraps the
        // normal proposer rather than replacing it.
        let proposer = crate::exchange_proposer::ExchangeProposerFactory::new(
            sc_basic_authorship::ProposerFactory::new(
                task_manager.spawn_handle(),
                client.clone(),
                transaction_pool.clone(),
                prometheus_registry.as_ref(),
                telemetry.as_ref().map(|x| x.handle()),
            ),
            exchange_pool.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
        );

        let client_clone = client.clone();
        // `babe_link` is `None` on a SunLead chain, so derive the slot duration from
        // the runtime config instead of the BABE link.
        let slot_duration = sc_consensus_babe::configuration(&*client)?.slot_duration();

        if use_sunlead {
            log::info!(target: "sunlead", "starting SunLead authoring (is_active = true)");
            let sunlead_params = SunLeadParams {
                keystore: keystore_container.keystore(),
                client: client.clone(),
                select_chain,
                env: proposer,
                block_import: sunlead_block_import,
                sync_oracle: sync_service.clone(),
                justification_sync_link: sync_service.clone(),
                create_inherent_data_providers: move |_, ()| {
                    async move {
                        let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
                        let slot =
							sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
								*timestamp,
								slot_duration,
							);
                        Ok((slot, timestamp))
                    }
                },
                force_authoring,
                backoff_authoring_blocks,
                genesis_hash,
                slot_duration,
                block_proposal_slot_portion: sc_consensus_slots::SlotProportion::new(0.5),
                max_block_proposal_slot_portion: None,
                telemetry: telemetry.as_ref().map(|x| x.handle()),
            };
            let sunlead = start_sunlead(sunlead_params);
            task_manager.spawn_essential_handle().spawn_blocking(
                "sunlead-proposer",
                Some("block-authoring"),
                sunlead,
            );
        } else {
            let babe_config = sc_consensus_babe::BabeParams {
                keystore: keystore_container.keystore(),
                client: client.clone(),
                select_chain,
                env: proposer,
                block_import: block_import
                    .clone()
                    .expect("BABE block import present when BABE is the active consensus"),
                sync_oracle: sync_service.clone(),
                justification_sync_link: sync_service.clone(),
                create_inherent_data_providers: move |parent, ()| {
                    let client_clone = client_clone.clone();
                    async move {
                        let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                        let slot =
							sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
								*timestamp,
								slot_duration,
							);

                        let storage_proof =
                            sp_transaction_storage_proof::registration::new_data_provider(
                                &*client_clone,
                                &parent,
                            )?;

                        Ok((slot, timestamp, storage_proof))
                    }
                },
                force_authoring,
                backoff_authoring_blocks,
                babe_link: babe_link
                    .clone()
                    .expect("BABE link present when BABE is the active consensus"),
                block_proposal_slot_portion: SlotProportion::new(0.5),
                max_block_proposal_slot_portion: None,
                telemetry: telemetry.as_ref().map(|x| x.handle()),
            };

            let babe = sc_consensus_babe::start_babe(babe_config)?;
            task_manager.spawn_essential_handle().spawn_blocking(
                "babe-proposer",
                Some("block-authoring"),
                babe,
            );
        }
    }

    // Spawn authority discovery module.
    if role.is_authority() {
        let authority_discovery_role =
            sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore());
        let dht_event_stream =
            network
                .event_stream("authority-discovery")
                .filter_map(|e| async move {
                    match e {
                        Event::Dht(e) => Some(e),
                        _ => None,
                    }
                });
        let (authority_discovery_worker, _service) =
            sc_authority_discovery::new_worker_and_service_with_config(
                sc_authority_discovery::WorkerConfig {
                    publish_non_global_ips: auth_disc_publish_non_global_ips,
                    public_addresses: auth_disc_public_addresses,
                    persisted_cache_directory: net_config_path,
                    ..Default::default()
                },
                client.clone(),
                Arc::new(network.clone()),
                Box::pin(dht_event_stream),
                authority_discovery_role,
                prometheus_registry.clone(),
                task_manager.spawn_handle(),
            );

        task_manager.spawn_handle().spawn(
            "authority-discovery-worker",
            Some("networking"),
            authority_discovery_worker.run(),
        );
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.keystore())
    } else {
        None
    };

    let grandpa_config = sc_consensus_grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: std::time::Duration::from_millis(333),
        justification_generation_period: GRANDPA_JUSTIFICATION_PERIOD,
        name: Some(name),
        observer_enabled: false,
        keystore,
        local_role: role,
        telemetry: telemetry.as_ref().map(|x| x.handle()),
        protocol_name: grandpa_protocol_name,
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_params = sc_consensus_grandpa::GrandpaParams {
            config: grandpa_config,
            link: grandpa_link,
            network: network.clone(),
            sync: Arc::new(sync_service.clone()),
            notification_service: grandpa_notification_service,
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            voting_rule: sc_consensus_grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry: prometheus_registry.clone(),
            shared_voter_state,
            offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool.clone()),
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            None,
            sc_consensus_grandpa::run_grandpa_voter(grandpa_params)?,
        );
    }

    Ok(NewFullBase {
        task_manager,
        client,
        network,
        sync: sync_service,
        transaction_pool,
        rpc_handlers,
    })
}

/// Builds a new service for a full client.
pub async fn new_full(config: Configuration,eth_config: EthConfiguration, cli: Cli) -> Result<TaskManager, ServiceError> {
    let res = new_full_base::<sc_network::NetworkWorker<_, _>>(config, eth_config,|_, _| ()).await;
        // .map(|NewFullBase { task_manager, .. }| task_manager).await;
		// if let Ok(v) = task_manager {
        //    Ok(v.task_manager)
		// } else {
		// 	panic!("Error starting service")
		// }
    match res {
        Ok(v) => Ok(v.task_manager),
        Err(e) => panic!("Error starting service: {:?}", e),
    }

    // let task_manager = match config.network.network_backend {
    // 	Some(sc_network::config::NetworkBackendType::Libp2p) => {
    // 			let task_manager = new_full_base::<sc_network::NetworkWorker<_, _>>(
    // 				config,
    // 				|_, _| (),
    // 			)
    // 			.map(|NewFullBase { task_manager, .. }| task_manager)?;
    // 			task_manager
    // 		},
    // 	Some(sc_network::config::NetworkBackendType::Litep2p) => {
    // 			let task_manager = new_full_base::<sc_network::Litep2pNetworkBackend>(
    // 				config,
    // 				|_, _| (),
    // 			)
    // 			.map(|NewFullBase { task_manager, .. }| task_manager)?;
    // 			task_manager
    // 		},
    // 	None => todo!(),
    // };

    // let database_path = config.database.path().map(Path::to_path_buf);
    // if let Some(database_path) = database_path {
    // 	sc_storage_monitor::StorageMonitorService::try_spawn(
    // 		cli.storage_monitor,
    // 		database_path,
    // 		&task_manager.spawn_essential_handle(),
    // 	)
    // 	.map_err(|e| ServiceError::Application(e.into()))?;
    // }

    // task_manager
    // Ok(task_manager)
}

// /// Builds a new service for a full client.
// pub fn new_full<
// 	N: sc_network::NetworkBackend<Block, <Block as sp_runtime::traits::Block>::Hash>,
// >(
// 	config: Configuration,
// ) -> Result<TaskManager, ServiceError> {
// 	let sc_service::PartialComponents {
// 		client,
// 		backend,
// 		mut task_manager,
// 		import_queue,
// 		keystore_container,
// 		select_chain,
// 		transaction_pool,
// 		other: (block_import, grandpa_link, mut telemetry),
// 	} = new_partial(&config)?;

// 	let mut net_config = sc_network::config::FullNetworkConfiguration::<
// 		Block,
// 		<Block as sp_runtime::traits::Block>::Hash,
// 		N,
// 	>::new(&config.network, config.prometheus_registry().cloned());
// 	let metrics = N::register_notification_metrics(config.prometheus_registry());

// 	let peer_store_handle = net_config.peer_store_handle();
// 	let grandpa_protocol_name = sc_consensus_grandpa::protocol_standard_name(
// 		&client.block_hash(0).ok().flatten().expect("Genesis block exists; qed"),
// 		&config.chain_spec,
// 	);
// 	let (grandpa_protocol_config, grandpa_notification_service) =
// 		sc_consensus_grandpa::grandpa_peers_set_config::<_, N>(
// 			grandpa_protocol_name.clone(),
// 			metrics.clone(),
// 			peer_store_handle,
// 		);
// 	net_config.add_notification_protocol(grandpa_protocol_config);

// 	let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
// 		backend.clone(),
// 		grandpa_link.shared_authority_set().clone(),
// 		Vec::default(),
// 	));

// 	let (network, system_rpc_tx, tx_handler_controller, sync_service) =
// 		sc_service::build_network(sc_service::BuildNetworkParams {
// 			config: &config,
// 			net_config,
// 			client: client.clone(),
// 			transaction_pool: transaction_pool.clone(),
// 			spawn_handle: task_manager.spawn_handle(),
// 			import_queue,
// 			block_announce_validator_builder: None,
// 			warp_sync_config: Some(WarpSyncConfig::WithProvider(warp_sync)),
// 			block_relay: None,
// 			metrics,
// 		})?;

// 	if config.offchain_worker.enabled {
// 		let offchain_workers =
// 			sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
// 				runtime_api_provider: client.clone(),
// 				is_validator: config.role.is_authority(),
// 				keystore: Some(keystore_container.keystore()),
// 				offchain_db: backend.offchain_storage(),
// 				transaction_pool: Some(OffchainTransactionPoolFactory::new(
// 					transaction_pool.clone(),
// 				)),
// 				network_provider: Arc::new(network.clone()),
// 				enable_http_requests: true,
// 				custom_extensions: |_| vec![],
// 			})?;
// 		task_manager.spawn_handle().spawn(
// 			"offchain-workers-runner",
// 			"offchain-worker",
// 			offchain_workers.run(client.clone(), task_manager.spawn_handle()).boxed(),
// 		);
// 	}

// 	let role = config.role;
// 	let force_authoring = config.force_authoring;
// 	let backoff_authoring_blocks: Option<()> = None;
// 	let name = config.network.node_name.clone();
// 	let enable_grandpa = !config.disable_grandpa;
// 	let prometheus_registry = config.prometheus_registry().cloned();

// 	info!(target: "sunchain", "Node role, : {}  enable_grandpa : {}", role, enable_grandpa);

// 	let rpc_extensions_builder = {
// 		let client = client.clone();
// 		let pool = transaction_pool.clone();

// 		Box::new(move |_| {
// 			let deps = crate::rpc::FullDeps { client: client.clone(), pool: pool.clone() };
// 			crate::rpc::create_full(deps).map_err(Into::into)
// 		})
// 	};

// 	let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
// 		network: Arc::new(network.clone()),
// 		client: client.clone(),
// 		keystore: keystore_container.keystore(),
// 		task_manager: &mut task_manager,
// 		transaction_pool: transaction_pool.clone(),
// 		rpc_builder: rpc_extensions_builder,
// 		backend,
// 		system_rpc_tx,
// 		tx_handler_controller,
// 		sync_service: sync_service.clone(),
// 		config,
// 		telemetry: telemetry.as_mut(),
// 	})?;

// 	if role.is_authority() {
// 		info!(target: "sunchain", "Starting AURA authoring");
// 		let proposer_factory = sc_basic_authorship::ProposerFactory::new(
// 			task_manager.spawn_handle(),
// 			client.clone(),
// 			transaction_pool.clone(),
// 			prometheus_registry.as_ref(),
// 			telemetry.as_ref().map(|x| x.handle()),
// 		);

// 		let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
// 		info!(target: "sunchain", "Sunchain Slot duration: {:?}", slot_duration);
// 		let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _>(
// 			StartAuraParams {
// 				slot_duration,
// 				client,
// 				select_chain,
// 				block_import,
// 				proposer_factory,
// 				create_inherent_data_providers: move |_, ()| async move {
// 					let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

// 					let slot =
// 						sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
// 							*timestamp,
// 							slot_duration,
// 						);

// 					Ok((slot, timestamp))
// 				},
// 				force_authoring,
// 				backoff_authoring_blocks,
// 				keystore: keystore_container.keystore(),
// 				sync_oracle: sync_service.clone(),
// 				justification_sync_link: sync_service.clone(),
// 				block_proposal_slot_portion: SlotProportion::new(2f32 / 3f32),
// 				max_block_proposal_slot_portion: None,
// 				telemetry: telemetry.as_ref().map(|x| x.handle()),
// 				compatibility_mode: Default::default(),
// 			},
// 		)?;

// 		// the AURA authoring task is considered essential, i.e. if it
// 		// fails we take down the service with it.
// 		task_manager
// 			.spawn_essential_handle()
// 			.spawn_blocking("aura", Some("block-authoring"), aura);
// 	}

// 	if enable_grandpa {
// 		// if the node isn't actively participating in consensus then it doesn't
// 		// need a keystore, regardless of which protocol we use below.
// 		let keystore = if role.is_authority() { Some(keystore_container.keystore()) } else { None };

// 		let grandpa_config = sc_consensus_grandpa::Config {
// 			// FIXME #1578 make this available through chainspec
// 			gossip_duration: Duration::from_millis(333),
// 			justification_generation_period: GRANDPA_JUSTIFICATION_PERIOD,
// 			name: Some(name),
// 			observer_enabled: false,
// 			keystore,
// 			local_role: role,
// 			telemetry: telemetry.as_ref().map(|x| x.handle()),
// 			protocol_name: grandpa_protocol_name,
// 		};

// 		// start the full GRANDPA voter
// 		// NOTE: non-authorities could run the GRANDPA observer protocol, but at
// 		// this point the full voter should provide better guarantees of block
// 		// and vote data availability than the observer. The observer has not
// 		// been tested extensively yet and having most nodes in a network run it
// 		// could lead to finality stalls.
// 		let grandpa_config = sc_consensus_grandpa::GrandpaParams {
// 			config: grandpa_config,
// 			link: grandpa_link,
// 			network,
// 			sync: Arc::new(sync_service),
// 			notification_service: grandpa_notification_service,
// 			voting_rule: sc_consensus_grandpa::VotingRulesBuilder::default().build(),
// 			prometheus_registry,
// 			shared_voter_state: SharedVoterState::empty(),
// 			telemetry: telemetry.as_ref().map(|x| x.handle()),
// 			offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(transaction_pool),
// 		};

// 		// the GRANDPA voter task is considered infallible, i.e.
// 		// if it fails we take down the service with it.
// 		task_manager.spawn_essential_handle().spawn_blocking(
// 			"grandpa-voter",
// 			None,
// 			sc_consensus_grandpa::run_grandpa_voter(grandpa_config)?,
// 		);
// 	}

// 	Ok(task_manager)
// }
