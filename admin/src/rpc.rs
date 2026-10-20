use anyhow::{Context, Result};
use sunchain_runtime::AccountId;
use scale_codec::Decode;
use jsonrpsee::{
    core::client::ClientT,
    rpc_params,
    ws_client::{WsClient, WsClientBuilder},
};
use serde::Deserialize;
use sp_core::{Bytes, H256};
use sp_runtime::generic::Era;

pub struct Rpc {
    client: WsClient,
}

#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub genesis_hash: H256,
    pub block_hash: H256,
    pub block_number: u64,
    pub spec_version: u32,
    pub transaction_version: u32,
    pub nonce: u32,
}

#[derive(Debug, Deserialize)]
struct RuntimeVersion {
    #[serde(rename = "specVersion")]
    spec_version: u32,
    #[serde(rename = "transactionVersion")]
    transaction_version: u32,
}

#[derive(Debug, Deserialize)]
struct Header {
    number: String,
}

impl Rpc {
    pub async fn connect(url: &str) -> Result<Self> {
        Ok(Self {
            client: WsClientBuilder::default().build(url).await?,
        })
    }

    pub async fn runtime_info(&self, account: AccountId, mortality: u64) -> Result<RuntimeInfo> {
        let genesis_hash: H256 = self
            .client
            .request("chain_getBlockHash", rpc_params![0u64])
            .await
            .context("chain_getBlockHash(0)")?;
        // A mortal era must be constructed from the current best block. The
        // runtime recomputes `Era::birth` from the block being validated; using
        // a lagging finalized head can produce a different birth block.
        let best_hash: H256 = self
            .client
            .request("chain_getBlockHash", rpc_params![Option::<u64>::None])
            .await
            .context("chain_getBlockHash(best)")?;
        let header: Header = self
            .client
            .request("chain_getHeader", rpc_params![best_hash])
            .await
            .context("chain_getHeader")?;
        let block_number = u64::from_str_radix(header.number.trim_start_matches("0x"), 16)?;
        let block_hash = if mortality == 0 {
            genesis_hash
        } else {
            let era = Era::mortal(mortality, block_number);
            let era_birth = era.birth(block_number);
            self.client
                .request("chain_getBlockHash", rpc_params![era_birth])
                .await
                .context("chain_getBlockHash(era birth)")?
        };
        let version: RuntimeVersion = self
            .client
            .request("state_getRuntimeVersion", rpc_params![best_hash])
            .await
            .context("state_getRuntimeVersion")?;
        let nonce: u32 = self
            .client
            .request("system_accountNextIndex", rpc_params![account])
            .await
            .context("system_accountNextIndex")?;
        Ok(RuntimeInfo {
            genesis_hash,
            block_hash,
            block_number,
            spec_version: version.spec_version,
            transaction_version: version.transaction_version,
            nonce,
        })
    }

    pub async fn submit_extrinsic(&self, encoded: &str) -> Result<H256> {
        Ok(self
            .client
            .request("author_submitExtrinsic", rpc_params![encoded])
            .await
            .context("author_submitExtrinsic")?)
    }

    pub async fn storage<T: Decode>(&self, key: Vec<u8>) -> Result<Option<T>> {
        let key = format!("0x{}", hex::encode(key));
        let raw: Option<Bytes> = self
            .client
            .request("state_getStorage", rpc_params![key])
            .await
            .context("state_getStorage")?;
        raw.map(|bytes| T::decode(&mut &bytes.0[..]).context("decode storage value"))
            .transpose()
    }

    pub async fn storage_value<T: Decode>(&self, pallet: &str, item: &str) -> Result<Option<T>> {
        let key = crate::query::storage_value_key(pallet, item);
        self.storage(key).await
    }

    pub async fn storage_map<T: Decode>(
        &self,
        pallet: &str,
        item: &str,
        encoded_key: &[u8],
        hasher: crate::query::MapHasher,
    ) -> Result<Option<T>> {
        let key = crate::query::map_key(pallet, item, encoded_key, hasher);
        self.storage(key).await
    }

    pub async fn storage_double_map<T: Decode>(
        &self,
        pallet: &str,
        item: &str,
        first: &[u8],
        second: &[u8],
        first_hasher: crate::query::MapHasher,
        second_hasher: crate::query::MapHasher,
    ) -> Result<Option<T>> {
        let key = crate::query::double_map_key(
            pallet,
            item,
            first,
            second,
            first_hasher,
            second_hasher,
        );
        self.storage(key).await
    }

    pub async fn pending_rewards(&self, era: u32, account: AccountId) -> Result<bool> {
        let best_hash: H256 = self
            .client
            .request("chain_getBlockHash", rpc_params![Option::<u64>::None])
            .await
            .context("chain_getBlockHash(best)")?;
        let args = (era, account);
        let encoded_args = format!("0x{}", hex::encode(scale_codec::Encode::encode(&args)));
        let raw: Bytes = self
            .client
            .request(
                "state_call",
                rpc_params!["StakingApi_pending_rewards", encoded_args, best_hash],
            )
            .await
            .context("state_call StakingApi_pending_rewards")?;
        bool::decode(&mut &raw.0[..]).context("decode pending rewards result")
    }
}
