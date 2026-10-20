use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::Arc,
};

use crate::{
	client::{Backend, Client},
	// keyring::*,
};
use codec::{Decode, Encode};
use futures::executor;
use sunchain_runtime::{
	constants::currency::DOLLARS, AccountId, BalancesCall, 
	RuntimeCall, Signature, SystemCall, UncheckedExtrinsic,Block,
};

use sc_block_builder::BlockBuilderBuilder;
use sc_client_api::{execution_extensions::ExecutionExtensions, UsageProvider};
use sc_client_db::PruningMode;
use sc_consensus::{BlockImport, BlockImportParams, ForkChoiceStrategy, ImportResult, ImportedAux};
use sc_executor::{WasmExecutionMethod, WasmtimeInstantiationStrategy};
use sp_api::ProvideRuntimeApi;
use sp_block_builder::BlockBuilder;
use sp_consensus::BlockOrigin;
use sp_core::{
	crypto::get_public_from_string_or_panic, ed25519, sr25519, traits::SpawnNamed, Pair,
};
use sp_crypto_hashing::blake2_256;
use sp_inherents::InherentData;
use sp_runtime::{
	generic::{self, ExtrinsicFormat, Preamble},
	traits::{Block as BlockT, IdentifyAccount, Verify},
	OpaqueExtrinsic,
};

/// Drop system cache.
///
/// Will panic if cache drop is impossible.
pub fn drop_system_cache() {
	#[cfg(target_os = "windows")]
	{
		log::warn!(
			target: "bench-logistics",
			"Clearing system cache on windows is not supported. Benchmark might totally be wrong.",
		);
		return
	}

	std::process::Command::new("sync")
		.output()
		.expect("Failed to execute system cache clear");

	#[cfg(target_os = "linux")]
	{
		log::trace!(target: "bench-logistics", "Clearing system cache...");
		std::process::Command::new("echo")
			.args(&["3", ">", "/proc/sys/vm/drop_caches", "2>", "/dev/null"])
			.output()
			.expect("Failed to execute system cache clear");

		let temp = tempfile::tempdir().expect("Failed to spawn tempdir");
		let temp_file_path = format!("of={}/buf", temp.path().to_string_lossy());

		// this should refill write cache with 2GB of garbage
		std::process::Command::new("dd")
			.args(&["if=/dev/urandom", &temp_file_path, "bs=64M", "count=32"])
			.output()
			.expect("Failed to execute dd for cache clear");

		// remove tempfile of previous command
		std::process::Command::new("rm")
			.arg(&temp_file_path)
			.output()
			.expect("Failed to remove temp file");

		std::process::Command::new("sync")
			.output()
			.expect("Failed to execute system cache clear");

		log::trace!(target: "bench-logistics", "Clearing system cache done!");
	}
    #[cfg(target_os = "macos")]
	{
		log::trace!(target: "bench-logistics", "Clearing system cache...");
		if let Err(err) = std::process::Command::new("purge").output() {
			log::error!("purge error {:?}: ", err);
			panic!("Could not clear system cache. Run under sudo?");
		}
		log::trace!(target: "bench-logistics", "Clearing system cache done!");
	}
}

/// Type of backend database.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DatabaseType {
	/// RocksDb backend.
	RocksDb,
	/// Parity DB backend.
	ParityDb,
}

impl DatabaseType {
	fn into_settings(self, path: PathBuf) -> sc_client_db::DatabaseSource {
		match self {
			Self::RocksDb => sc_client_db::DatabaseSource::RocksDb { path, cache_size: 512 },
			Self::ParityDb => sc_client_db::DatabaseSource::ParityDb { path },
		}
	}
}

/// Uses multiple threads as the regular executable.
#[derive(Debug, Clone)]
pub struct TaskExecutor {
	pool: executor::ThreadPool,
}

impl TaskExecutor {
	fn new() -> Self {
		Self { pool: executor::ThreadPool::new().expect("Failed to create task executor") }
	}
}

impl SpawnNamed for TaskExecutor {
	fn spawn(
		&self,
		_: &'static str,
		_: Option<&'static str>,
		future: futures::future::BoxFuture<'static, ()>,
	) {
		self.pool.spawn_ok(future);
	}

	fn spawn_blocking(
		&self,
		_: &'static str,
		_: Option<&'static str>,
		future: futures::future::BoxFuture<'static, ()>,
	) {
		self.pool.spawn_ok(future);
	}
}


struct Guard(tempfile::TempDir);

impl Guard {
	fn path(&self) -> &Path {
		self.0.path()
	}
}

/// Benchmarking/test context holding instantiated client and backend references.
pub struct BenchContext {
	/// Node client.
	pub client: Arc<Client>,
	/// Node backend.
	pub backend: Arc<Backend>,
	/// Spawn handle.
	pub spawn_handle: Box<dyn SpawnNamed>,

	db_guard: Guard,
}

type AccountPublic = <Signature as Verify>::Signer;

impl BenchContext {
	/// Import some block.
	pub fn import_block(&mut self, block: Block) {
		let mut import_params =
			BlockImportParams::new(BlockOrigin::NetworkBroadcast, block.header.clone());
		import_params.body = Some(block.extrinsics().to_vec());
		import_params.fork_choice = Some(ForkChoiceStrategy::LongestChain);

		assert_eq!(self.client.chain_info().best_number, 0);

		assert_eq!(
			futures::executor::block_on(self.client.import_block(import_params))
				.expect("Failed to import block"),
			ImportResult::Imported(ImportedAux {
				header_only: false,
				clear_justification_requests: false,
				needs_justification: false,
				bad_justification: false,
				is_new_best: true,
			})
		);

		assert_eq!(self.client.chain_info().best_number, 1);
	}

	/// Database path for the current context.
	pub fn path(&self) -> &Path {
		self.db_guard.path()
	}
}
