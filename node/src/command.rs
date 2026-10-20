use crate::{
	benchmarking::{inherent_benchmark_data, RemarkBuilder, TransferKeepAliveBuilder},
	chain_spec::{self, sunchain_spec},
	cli::{ Cli, Subcommand},
	service,
};
use frame_benchmarking_cli::{BenchmarkCmd, ExtrinsicFactory, SUBSTRATE_REFERENCE_HARDWARE};
use sc_cli::SubstrateCli;
use sc_service::PartialComponents;
use sunchain_runtime::{Block, EXISTENTIAL_DEPOSIT, apis::RuntimeApi};
use ecdsa_keyring::Keyring;
use sp_application_crypto::Pair;
impl SubstrateCli for Cli {
	fn impl_name() -> String {
		"Sunchain Node".into()
	}

	fn impl_version() -> String {
		env!("SUBSTRATE_CLI_IMPL_VERSION").into()
	}

	fn description() -> String {
		env!("CARGO_PKG_DESCRIPTION").into()
	}

	fn author() -> String {
		env!("CARGO_PKG_AUTHORS").into()
	}

	fn support_url() -> String {
		"support.anonymous.an".into()
	}

	fn copyright_start_year() -> i32 {
		2017
	}

	fn load_spec(&self, id: &str) -> Result<Box<dyn sc_service::ChainSpec>, String> {
		Ok(match id {
			"dev" => Box::new(chain_spec::development_chain_spec()?),
			"" | "local" => Box::new(chain_spec::local_chain_spec()?),
			"sunchain" =>
			{
				 println!("load sunchain spec");
				 Box::new(sunchain_spec::sun_chain_spec()?)
			}
			path =>
				Box::new(chain_spec::ChainSpec::from_json_file(std::path::PathBuf::from(path))?),
		})
	}
}

/// Parse and run command line arguments
pub fn run<I>(args: Option<I>) -> sc_cli::Result<()> 
where I: IntoIterator,
	I::Item: Into<std::ffi::OsString> + Clone,
{
	let cli = args.map_or(Cli::from_args(),
		|args| Cli::from_iter(args));

	match &cli.subcommand {
		Some(Subcommand::Inspect(cmd)) => {
			let runner = cli.create_runner(cmd)?;

			runner.sync_run(|config| cmd.run::<Block, RuntimeApi>(config))
		},
		Some(Subcommand::Key(cmd)) => cmd.run(&cli),
		Some(Subcommand::BuildSpec(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.sync_run(|config| cmd.run(config.chain_spec, config.network))
		},
		Some(Subcommand::CheckBlock(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.async_run(|config| {
				let PartialComponents { client, task_manager, import_queue, .. } =
					service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
				Ok((cmd.run(client, import_queue), task_manager))
			})
		},
		Some(Subcommand::ExportBlocks(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.async_run(|config| {
				let PartialComponents { client, task_manager, .. } = service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
				Ok((cmd.run(client, config.database), task_manager))
			})
		},
		Some(Subcommand::ExportState(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.async_run(|config| {
				let PartialComponents { client, task_manager, .. } = service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
				Ok((cmd.run(client, config.chain_spec), task_manager))
			})
		},
		Some(Subcommand::ImportBlocks(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.async_run(|config| {
				let PartialComponents { client, task_manager, import_queue, .. } =
					service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
				Ok((cmd.run(client, import_queue), task_manager))
			})
		},
		Some(Subcommand::PurgeChain(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.sync_run(|config| cmd.run(config.database))
		},
		Some(Subcommand::Revert(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.async_run(|config| {
				let PartialComponents { client, task_manager, backend, .. } =
					service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
				let aux_revert = Box::new(|client, _, blocks| {
					sc_consensus_grandpa::revert(client, blocks)?;
					Ok(())
				});
				Ok((cmd.run(client, backend, Some(aux_revert)), task_manager))
			})
		},
		Some(Subcommand::Benchmark(cmd)) => {
			let runner = cli.create_runner(cmd)?;

			runner.sync_run(|config| {
				// This switch needs to be in the client, since the client decides
				// which sub-commands it wants to support.
				match cmd {
					BenchmarkCmd::Pallet(cmd) => {
						if !cfg!(feature = "runtime-benchmarks") {
							return Err(
								"Runtime benchmarking wasn't enabled when building the node. \
							You can enable it with `--features runtime-benchmarks`."
									.into(),
							);
						}

						// The second type parameter is the *extra* host functions tuple the
						// benchmark executor must register. Passing `()` would leave
						// `ext_storage_proof_size_storage_proof_size_version_1` unregistered and
						// every runtime call would fail to instantiate the wasm.
						cmd.run_with_spec::<
							sp_runtime::traits::HashingFor<Block>,
							service::HostFunctions,
						>(Some(config.chain_spec))
					},
					BenchmarkCmd::Block(cmd) => {
						let PartialComponents { client, .. } = service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
						cmd.run(client)
					},
					#[cfg(not(feature = "runtime-benchmarks"))]
					BenchmarkCmd::Storage(_) => Err(
						"Storage benchmarking can be enabled with `--features runtime-benchmarks`."
							.into(),
					),
					#[cfg(feature = "runtime-benchmarks")]
					BenchmarkCmd::Storage(cmd) => {
						let PartialComponents { client, backend, .. } =
							service::new_partial::<sc_network::NetworkWorker<_, _>>(&config, &cli.eth)?;
						let db = backend.expose_db();
						let storage = backend.expose_storage();

						cmd.run(config, client, db, storage, None)
					},
					// BenchmarkCmd::Overhead(cmd) => {
					// 	let PartialComponents { client, .. } = service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
					// 	let ext_builder = RemarkBuilder::new(client.clone());

					// 	cmd.run(
					// 		config.chain_spec.name().into(),
					// 		client,
					// 		inherent_benchmark_data()?,
					// 		Vec::new(),
					// 		&ext_builder,
					// 		false,
					// 	)
					// },
					// BenchmarkCmd::Extrinsic(cmd) => {
					// 	let PartialComponents { client, .. } = service::new_partial::<sc_network::NetworkWorker<_, _>>(&config,&cli.eth)?;
					// 	// Register the *Remark* and *TKA* builders.
					// 	let ext_factory = ExtrinsicFactory(vec![
					// 		Box::new(RemarkBuilder::new(client.clone())),
					// 		Box::new(TransferKeepAliveBuilder::new(
					// 			client.clone(),
					// 			Keyring::Alith.pair().public().into(),
					// 			EXISTENTIAL_DEPOSIT,
					// 		)),
					// 	]);

					// 	cmd.run(client, inherent_benchmark_data()?, Vec::new(), &ext_factory)
					// },
					BenchmarkCmd::Machine(cmd) =>
						cmd.run(&config, SUBSTRATE_REFERENCE_HARDWARE.clone()),
					// `Overhead` and `Extrinsic` are not wired up in this node.
					#[allow(unreachable_patterns)]
					_ => Err("Unsupported benchmarking command".into()),
				}
			})
		},
		Some(Subcommand::ChainInfo(cmd)) => {
			let runner = cli.create_runner(cmd)?;
			runner.sync_run(|config| cmd.run::<Block>(&config))
		},
		None => {
			let runner = cli.create_runner(&cli.run)?;
			runner.run_node_until_exit(|config| async move {
				match config.network.network_backend {
					// sc_network::config::NetworkBackendType::Libp2p => service::new_full::<
					// 	sc_network::NetworkWorker<
					// 		sunchain_runtime::opaque::Block,
					// 		<sunchain_runtime::opaque::Block as sp_runtime::traits::Block>::Hash,
					// 	>,
					// >(config)
					// .map_err(sc_cli::Error::Service),
					// sc_network::config::NetworkBackendType::Litep2p =>
					// 	service::new_full::<sc_network::Litep2pNetworkBackend>(config)
					// 		.map_err(sc_cli::Error::Service),
					_ => service::new_full(config,cli.eth.clone() ,cli).await
						.map_err(sc_cli::Error::Service),
				}
			})
		},
	}
}
