mod calls;
mod query;
mod rpc;
mod signer;
mod tx_builder;

use anyhow::Result;
use calls::{base_fee_call, payout_stakers_call};
use clap::{Args, Parser, Subcommand};
use query::{QueryCommand, StakingQuery};
use rpc::Rpc;
use sp_core::U256;

#[derive(Debug, Parser)]
#[command(name = "sun-admin", about = "SunChain runtime transaction builder")]
struct Cli {
    #[arg(long, env = "SUN_ADMIN_RPC", default_value = "ws://127.0.0.1:9944")]
    rpc: String,
    #[arg(long, env = "SUN_ADMIN_PRIVATE_KEY")]
    private_key: Option<String>,
    #[arg(long, default_value_t = 0)]
    tip: u128,
    #[arg(
        long,
        default_value_t = 64,
        help = "Mortal era period; use 0 for an immortal transaction"
    )]
    mortality: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Set BaseFeePerGas through Sudo.
    BaseFee(BaseFeeCommand),
    /// Claim a staking era reward. Any signed account may submit this call.
    Staking(StakingCommand),
    /// Query read-only chain state.
    Query(QueryCommandArgs),
}

#[derive(Debug, Args)]
struct QueryCommandArgs {
    #[command(subcommand)]
    command: QuerySubcommand,
}

#[derive(Debug, Subcommand)]
enum QuerySubcommand {
    /// Query staking state.
    Staking(StakingQueryArgs),
    /// Query a native SUN balance.
    Balance { account: String },
    /// Query BaseFeePerGas and Elasticity.
    #[command(name = "base-fee")]
    BaseFee,
    /// Query the current Sudo account.
    Sudo,
}

#[derive(Debug, Args)]
struct StakingQueryArgs {
    #[command(subcommand)]
    command: StakingQuerySubcommand,
}

#[derive(Debug, Subcommand)]
enum StakingQuerySubcommand {
    #[command(name = "current-era")]
    CurrentEra,
    Validator { validator: String },
    #[command(name = "pending-rewards")]
    PendingRewards { validator: String, era: u32 },
    #[command(name = "eras-stakers-overview")]
    ErasStakersOverview { validator: String, era: u32 },
}

#[derive(Debug, Args)]
struct BaseFeeCommand {
    #[command(subcommand)]
    command: BaseFeeSubcommand,
}

#[derive(Debug, Subcommand)]
enum BaseFeeSubcommand {
    #[command(name = "set-base-fee-per-gas")]
    SetBaseFeePerGas { fee: String },
    #[command(name = "set-elasticity")]
    SetElasticity { parts_per_million: u32 },
}

#[derive(Debug, Args)]
struct StakingCommand {
    #[command(subcommand)]
    command: StakingSubcommand,
}

#[derive(Debug, Subcommand)]
enum StakingSubcommand {
    #[command(name = "payout-stakers")]
    PayoutStakers { validator: String, era: u32 },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let rpc = Rpc::connect(&cli.rpc).await?;
    if let Command::Query(command) = cli.command {
        let command = match command.command {
            QuerySubcommand::Staking(command) => match command.command {
                StakingQuerySubcommand::CurrentEra => {
                    QueryCommand::Staking(StakingQuery::CurrentEra)
                }
                StakingQuerySubcommand::Validator { validator } => {
                    QueryCommand::Staking(StakingQuery::Validator { validator })
                }
                StakingQuerySubcommand::PendingRewards { validator, era } => {
                    QueryCommand::Staking(StakingQuery::PendingRewards { validator, era })
                }
                StakingQuerySubcommand::ErasStakersOverview { validator, era } => {
                    QueryCommand::Staking(StakingQuery::ErasStakersOverview { validator, era })
                }
            },
            QuerySubcommand::Balance { account } => QueryCommand::Balance { account },
            QuerySubcommand::BaseFee => QueryCommand::BaseFee,
            QuerySubcommand::Sudo => QueryCommand::Sudo,
        };
        return query::run(&rpc, command).await;
    }

    let signer = signer::EthereumSigner::from_uri(
        cli.private_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--private-key is required for transaction commands"))?,
    )?;

    let call = match cli.command {
        Command::BaseFee(command) => match command.command {
            BaseFeeSubcommand::SetBaseFeePerGas { fee } => {
                base_fee_call::set_base_fee_per_gas(parse_u256(&fee)?)
            }
            BaseFeeSubcommand::SetElasticity { parts_per_million } => {
                base_fee_call::set_elasticity(parts_per_million)?
            }
        },
        Command::Staking(command) => match command.command {
            StakingSubcommand::PayoutStakers { validator, era } => {
                payout_stakers_call::payout_stakers(signer::parse_account(&validator)?, era)
            }
        },
        Command::Query(_) => unreachable!("query commands return before transaction construction"),
    };

    let info = rpc.runtime_info(signer.account_id(), cli.mortality).await?;
    let extrinsic =
        tx_builder::build_signed_extrinsic(call, signer, &info, cli.tip, cli.mortality)?;
    let encoded = format!("0x{}", hex::encode(scale_codec::Encode::encode(&extrinsic)));
    let hash = rpc.submit_extrinsic(&encoded).await?;
    println!("submitted: 0x{}", hex::encode(hash.as_bytes()));
    Ok(())
}

fn parse_u256(value: &str) -> Result<U256> {
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(U256::from_str_radix(hex, 16)?)
    } else {
        Ok(U256::from_dec_str(value)?)
    }
}
