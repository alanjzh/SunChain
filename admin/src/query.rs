use anyhow::{Context, Result};
use sunchain_runtime::{AccountId, Balance, Nonce};
use pallet_staking::ValidatorPrefs;
use scale_codec::Encode;
use sp_core::{H160, U256};
use sp_io::hashing::{blake2_128, twox_128, twox_64};
use sp_runtime::{Perbill, Permill};
use sp_staking::PagedExposureMetadata;

use crate::{rpc::Rpc, signer};

pub enum QueryCommand {
    Staking(StakingQuery),
    Balance { account: String },
    BaseFee,
    Sudo,
}

pub enum StakingQuery {
    CurrentEra,
    Validator { validator: String },
    PendingRewards { validator: String, era: u32 },
    ErasStakersOverview { validator: String, era: u32 },
}

pub async fn run(rpc: &Rpc, command: QueryCommand) -> Result<()> {
    match command {
        QueryCommand::Staking(command) => match command {
            StakingQuery::CurrentEra => {
                let era: Option<u32> = rpc
                    .storage_value("Staking", "CurrentEra")
                    .await?;
                match era {
                    Some(era) => println!("current-era: {era}"),
                    None => println!("current-era: none"),
                }
            }
            StakingQuery::Validator { validator } => {
                let account = parse_account(&validator)?;
                let prefs: ValidatorPrefs = rpc
                    .storage_map("Staking", "Validators", account.as_bytes(), MapHasher::Twox64)
                    .await?
                    .context("address is not in staking.validators")?;
                println!("validator: {account}");
                println!("commission: {}", perbill_parts(prefs.commission));
                println!("blocked: {}", prefs.blocked);
            }
            StakingQuery::PendingRewards { validator, era } => {
                let account = parse_account(&validator)?;
                // This is the same runtime API exposed by the runtime. It avoids
                // depending on the internal staking reward storage layout.
                let pending = rpc.pending_rewards(era, account.into()).await?;
                println!("validator: {account}");
                println!("era: {era}");
                println!("pending-rewards: {pending}");
            }
            StakingQuery::ErasStakersOverview { validator, era } => {
                let account = parse_account(&validator)?;
                let overview: Option<PagedExposureMetadata<Balance>> = rpc
                    .storage_double_map(
                        "Staking",
                        "ErasStakersOverview",
                        &era.encode(),
                        &account.encode(),
                        MapHasher::Twox64,
                        MapHasher::Twox64,
                    )
                    .await?;
                println!("validator: {account}");
                println!("era: {era}");
                match overview {
                    Some(overview) => {
                        println!("exposure: present");
                        println!("total: {}", overview.total);
                        println!("own: {}", overview.own);
                        println!("nominator-count: {}", overview.nominator_count);
                        println!("page-count: {}", overview.page_count);
                    }
                    None => println!("exposure: none"),
                }
            }
        },
        QueryCommand::Balance { account } => {
            let account = parse_account(&account)?;
            let info: frame_system::AccountInfo<Nonce, pallet_balances::AccountData<Balance>> = rpc
                .storage_map("System", "Account", account.as_bytes(), MapHasher::Blake2_128)
                .await?
                .unwrap_or_default();
            println!("account: {account}");
            println!("free: {}", info.data.free);
            println!("reserved: {}", info.data.reserved);
            println!("frozen: {}", info.data.frozen);
            println!("nonce: {}", info.nonce);
        }
        QueryCommand::BaseFee => {
            let fee: U256 = rpc.storage_value("BaseFee", "BaseFeePerGas").await?.unwrap_or_default();
            let elasticity: Permill = rpc
                .storage_value("BaseFee", "Elasticity")
                .await?
                .unwrap_or_else(Permill::zero);
            println!("base-fee-per-gas: {fee}");
            println!("elasticity-parts-per-million: {}", elasticity.deconstruct());
            println!("elasticity: {}", elasticity.deconstruct() as f64 / 1_000_000.0);
        }
        QueryCommand::Sudo => {
            let key: Option<AccountId> = rpc.storage_value("Sudo", "Key").await?;
            match key {
                Some(key) => println!("sudo-key: {key}"),
                None => println!("sudo-key: none"),
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum MapHasher {
    Twox64,
    Blake2_128,
}

fn storage_prefix(pallet: &str, item: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(&twox_128(pallet.as_bytes()));
    key.extend_from_slice(&twox_128(item.as_bytes()));
    key
}

pub(crate) fn map_key(pallet: &str, item: &str, encoded_key: &[u8], hasher: MapHasher) -> Vec<u8> {
    let mut key = storage_prefix(pallet, item);
    match hasher {
        MapHasher::Twox64 => key.extend_from_slice(&twox_64(encoded_key)),
        MapHasher::Blake2_128 => key.extend_from_slice(&blake2_128(encoded_key)),
    }
    key.extend_from_slice(encoded_key);
    key
}

pub(crate) fn double_map_key(
    pallet: &str,
    item: &str,
    first: &[u8],
    second: &[u8],
    first_hasher: MapHasher,
    second_hasher: MapHasher,
) -> Vec<u8> {
    let mut key = storage_prefix(pallet, item);
    append_hashed_key(&mut key, first, first_hasher);
    append_hashed_key(&mut key, second, second_hasher);
    key
}

fn append_hashed_key(key: &mut Vec<u8>, encoded_key: &[u8], hasher: MapHasher) {
    match hasher {
        MapHasher::Twox64 => key.extend_from_slice(&twox_64(encoded_key)),
        MapHasher::Blake2_128 => key.extend_from_slice(&blake2_128(encoded_key)),
    }
    key.extend_from_slice(encoded_key);
}

fn value_key(pallet: &str, item: &str) -> Vec<u8> {
    storage_prefix(pallet, item)
}

fn parse_account(value: &str) -> Result<H160> {
    signer::parse_account(value)
}

fn perbill_parts(value: Perbill) -> u32 {
    value.deconstruct()
}

pub(crate) fn storage_value_key(pallet: &str, item: &str) -> Vec<u8> {
    value_key(pallet, item)
}
