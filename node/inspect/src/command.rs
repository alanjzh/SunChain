//! Command ran by the CLI

use crate::{
	cli::{InspectCmd, InspectSubCmd},
	Inspector,
};
use sc_cli::{CliConfiguration, ImportParams, Result, SharedParams};
use sc_service::Configuration;
use sp_runtime::traits::Block;

type HostFunctions =
	(sp_io::SubstrateHostFunctions);

impl InspectCmd {
	/// Run the inspect command, passing the inspector.
	pub fn run<B, RA>(&self, config: Configuration) -> Result<()>
	where
		B: Block,
		RA: Send + Sync + 'static,
	{
		let executor = sc_service::new_wasm_executor::<HostFunctions>(&config.executor);
		let client = sc_service::new_full_client::<B, RA, _>(&config, None, executor)?;
		let inspect = Inspector::<B>::new(client);

		match &self.command {
			InspectSubCmd::Block { input } => {
				let input = input.parse()?;
				let res = inspect.block(input).map_err(|e| e.to_string())?;
				println!("{res}");
				Ok(())
			},
			InspectSubCmd::Extrinsic { input } => {
				let input = input.parse()?;
				let res = inspect.extrinsic(input).map_err(|e| e.to_string())?;
				println!("{res}");
				Ok(())
			},
		}
	}
}

impl CliConfiguration for InspectCmd {
	fn shared_params(&self) -> &SharedParams {
		&self.shared_params
	}

	fn import_params(&self) -> Option<&ImportParams> {
		Some(&self.import_params)
	}
}
