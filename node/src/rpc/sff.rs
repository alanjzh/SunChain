//! SFF fast-confirmation RPCs (design §6 F3, §5 `node/src/rpc`).
//!
//! Everything here is **explicitly labelled as revocable**. GRANDPA remains the
//! authoritative finality in F0--F3, so a fast confirmation is a latency
//! optimisation for UIs and indexers — never a substitute for `finalizedHead`
//! in clearing, withdrawals, bridges or asset accounting (F3 gate).
//!
//! * `sff_status` — node-level status: is a confirmation being served, has SFF
//!   diverged from GRANDPA, and a monotonic `revision` consumers can watch.
//! * `sff_fastConfirmedHead` — the current fast-confirmed block, or `null` when
//!   nothing is served (no certificate yet, or suspended after a divergence).
//! * `sff_certificate` — the CommitQC recorded for one height.
//! * `sff_subscribeFastConfirmed` — WS subscription that pushes a status update
//!   whenever the view changes (new confirmation, GRANDPA catching up, degrade
//!   or recovery). This is the reorg/降级 interface indexers reconcile against.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use jsonrpsee::{
    core::{async_trait, server::PendingSubscriptionSink},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use sc_client_api::HeaderBackend;
use sc_rpc::{
    utils::{spawn_subscription_task, BoundedVecDeque, PendingSubscription},
    SubscriptionTaskExecutor,
};

use sunchain_runtime::opaque::Block;

use crate::fast_finality_view::{
    ChainView, ClientChainView, FastConfirmedView, FastFinalityView, SffStatus,
};

/// How often the subscription re-checks the view.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[rpc(client, server)]
pub trait SffRpcApi {
    /// Node-level status of the SFF fast-confirmation surface.
    #[method(name = "sff_status")]
    fn status(&self) -> Result<SffStatus, ErrorObjectOwned>;

    /// The current fast-confirmed head, or `null` when none is served.
    #[method(name = "sff_fastConfirmedHead")]
    fn fast_confirmed_head(&self) -> Result<Option<FastConfirmedView>, ErrorObjectOwned>;

    /// The CommitQC recorded for `height`, or `null`.
    #[method(name = "sff_certificate")]
    fn certificate(&self, height: u32) -> Result<Option<FastConfirmedView>, ErrorObjectOwned>;

    /// Subscribe to status updates (new confirmation, degrade, recovery, reorg).
    #[subscription(
        name = "sff_subscribeFastConfirmed" => "sff_fastConfirmed",
        unsubscribe = "sff_unsubscribeFastConfirmed",
        item = SffStatus
    )]
    fn subscribe_fast_confirmed(&self);
}

pub struct SffRpc<C> {
    client: Arc<C>,
    view: Arc<FastFinalityView>,
    executor: SubscriptionTaskExecutor,
}

impl<C> SffRpc<C> {
    pub fn new(
        client: Arc<C>,
        view: Arc<FastFinalityView>,
        executor: SubscriptionTaskExecutor,
    ) -> Self {
        Self {
            client,
            view,
            executor,
        }
    }
}

fn rpc_err(e: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32010, e.to_string(), None::<()>)
}

#[async_trait]
impl<C> SffRpcApiServer for SffRpc<C>
where
    C: HeaderBackend<Block> + Send + Sync + 'static,
{
    fn status(&self) -> Result<SffStatus, ErrorObjectOwned> {
        let chain = ClientChainView::<C, Block>::new(self.client.clone());
        Ok(self.view.status(&chain))
    }

    fn fast_confirmed_head(&self) -> Result<Option<FastConfirmedView>, ErrorObjectOwned> {
        let chain = ClientChainView::<C, Block>::new(self.client.clone());
        Ok(self.view.head(&chain))
    }

    fn certificate(&self, height: u32) -> Result<Option<FastConfirmedView>, ErrorObjectOwned> {
        let chain = ClientChainView::<C, Block>::new(self.client.clone());
        Ok(self.view.certificate(&chain, height))
    }

    fn subscribe_fast_confirmed(&self, pending: PendingSubscriptionSink) {
        let view = self.view.clone();
        let chain: Arc<dyn ChainView> =
            Arc::new(ClientChainView::<C, Block>::new(self.client.clone()));

        // Push only on change: consumers watch `revision` to detect new
        // confirmations, GRANDPA catching up, degrade and recovery.
        let stream = futures::stream::unfold(None::<u64>, move |last| {
            let view = view.clone();
            let chain = chain.clone();
            async move {
                loop {
                    tokio::time::sleep(POLL_INTERVAL).await;
                    let status = view.status(chain.as_ref());
                    let revision = status.revision;
                    if Some(revision) != last {
                        return Some((status, Some(revision)));
                    }
                }
            }
        })
        .boxed();

        spawn_subscription_task(
            &self.executor,
            PendingSubscription::from(pending).pipe_from_stream(stream, BoundedVecDeque::default()),
        );
    }
}
