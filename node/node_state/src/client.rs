//! Utilities to build a `TestClient` for `Sunchain-runtime`.

use sp_runtime::BuildStorage;
/// Re-export test-client utilities.
pub use substrate_test_utils::*;

/// Call executor for `kitchensink-runtime` `TestClient`.
// use node_cli::service::RuntimeExecutor;
use sunchain_runtime::{Block, apis::RuntimeApi};
/// Default backend type.
pub type Backend = sc_client_db::Backend<Block>;

/// Test client type.
pub type Client = client::Client<
	Backend,
	client::LocalCallExecutor<Block, Backend, RuntimeExecutor>,
	Block,
	RuntimeApi,
>;

pub type HostFunctions = sp_io::SubstrateHostFunctions;
	// (sp_io::SubstrateHostFunctions, sp_statement_store::runtime_api::HostFunctions);
pub type RuntimeExecutor = sc_executor::WasmExecutor<HostFunctions>;

/// Genesis configuration parameters for `TestClient`.
#[derive(Default)]
pub struct GenesisParameters;

impl substrate_test_client::GenesisInit for GenesisParameters {
	fn genesis_storage(&self) -> Storage {
		let mut storage = crate::genesis::config().build_storage().unwrap();
		storage.top.insert(
			sp_core::storage::well_known_keys::CODE.to_vec(),
			kitchensink_runtime::wasm_binary_unwrap().into(),
		);
		storage
	}
}

/// A `test-runtime` extensions to `TestClientBuilder`.
pub trait TestClientBuilderExt: Sized {
	/// Create test client builder.
	fn new() -> Self;

	/// Build the test client.
	fn build(self) -> Client;
}

impl TestClientBuilderExt
	for substrate_test_client::TestClientBuilder<
		node_primitives::Block,
		client::LocalCallExecutor<node_primitives::Block, Backend, RuntimeExecutor>,
		Backend,
		GenesisParameters,
	>
{
	fn new() -> Self {
		Self::default()
	}
	fn build(self) -> Client {
		let executor = RuntimeExecutor::builder().build();
		use sc_service::client::LocalCallExecutor;
		use std::sync::Arc;
		let executor = LocalCallExecutor::new(
			self.backend().clone(),
			executor.clone(),
			Default::default(),
			ExecutionExtensions::new(None, Arc::new(executor)),
		)
		.expect("Creates LocalCallExecutor");
		self.build_with_executor(executor).0
	}
}
