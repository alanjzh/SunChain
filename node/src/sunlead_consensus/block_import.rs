//! SunLead block import.
//!
//! Wraps the underlying block import (Frontier -> Grandpa) and only adds a
//! thin layer. The actual pre-digest verification happens in `SunLeadVerifier`
//! (running inside the import queue, *before* the runtime executes the block),
//! so the `SunLeadBlockImport` itself just delegates import/check to the inner
//! import.

use sc_consensus::block_import::ForkChoiceStrategy;
use sc_consensus::{BlockImport as BlockImportT, BlockImportParams, ImportResult};
use sp_consensus::Error as ConsensusError;
use sp_runtime::traits::Block as BlockT;
use std::marker::PhantomData;

/// A `BlockImport` that wraps the chain's effective block import and verifies the
/// SunLead pre-digest *before* the runtime executes the block (via the verifier
/// in the import queue).
#[derive(Clone)]
pub struct SunLeadBlockImport<B: BlockT, I> {
	inner: I,
	_phantom: PhantomData<B>,
}

impl<B: BlockT, I> SunLeadBlockImport<B, I> {
	/// Create a new SunLead block import wrapping `inner`.
	pub fn new(inner: I) -> Self {
		Self { inner, _phantom: PhantomData }
	}

	/// Access the inner block import (used by the worker when importing the
	/// block it authored).
	pub fn inner(&self) -> &I {
		&self.inner
	}
}

#[async_trait::async_trait]
impl<B: BlockT, I> BlockImportT<B> for SunLeadBlockImport<B, I>
where
	I: BlockImportT<B> + Send + Sync,
	I::Error: Into<ConsensusError>,
{
	type Error = ConsensusError;

	async fn check_block(
		&self,
		block: sc_consensus::BlockCheckParams<B>,
	) -> Result<ImportResult, Self::Error> {
		self.inner.check_block(block).await.map_err(Into::into)
	}

	async fn import_block(
		&self,
		mut block: BlockImportParams<B>,
	) -> Result<ImportResult, Self::Error> {
		// Unlike the BABE path (whose `BabeBlockImport` sets the fork choice),
		// the SunLead path has no authority block import to fill in
		// `fork_choice`. If it reaches the bottom `Client` importer as `None`
		// the import fails with `IncompletePipeline` and every authored block
		// is rejected. Set the standard solo-chain rule here.
		if block.fork_choice.is_none() {
			block.fork_choice = Some(ForkChoiceStrategy::LongestChain);
		}
		self.inner.import_block(block).await.map_err(Into::into)
	}
}
