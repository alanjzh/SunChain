use anyhow::{bail, Result};
use sunchain_runtime::RuntimeCall;
use pallet_base_fee::Call as BaseFeeCall;
use pallet_staking::Call as StakingCall;
use pallet_sudo::Call as SudoCall;
use sp_core::{H160, U256};
use sp_runtime::Permill;

pub mod base_fee_call {
    use super::*;

    pub fn set_base_fee_per_gas(fee: U256) -> RuntimeCall {
        let inner = RuntimeCall::BaseFee(BaseFeeCall::set_base_fee_per_gas { fee });
        RuntimeCall::Sudo(SudoCall::sudo {
            call: Box::new(inner),
        })
    }

    pub fn set_elasticity(parts_per_million: u32) -> Result<RuntimeCall> {
        let elasticity = Permill::from_parts(parts_per_million);
        if parts_per_million > 1_000_000 {
            bail!("elasticity must be <= 1_000_000 parts per million");
        }
        let inner = RuntimeCall::BaseFee(BaseFeeCall::set_elasticity { elasticity });
        Ok(RuntimeCall::Sudo(SudoCall::sudo {
            call: Box::new(inner),
        }))
    }
}

pub mod payout_stakers_call {
    use super::*;

    pub fn payout_stakers(validator_stash: H160, era: u32) -> RuntimeCall {
        RuntimeCall::Staking(StakingCall::payout_stakers {
            validator_stash: validator_stash.into(),
            era,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{base_fee_call, payout_stakers_call};
    use sunchain_runtime::RuntimeCall;
    use scale_codec::Encode;
    use sp_core::{H160, U256};

    #[test]
    fn base_fee_calls_are_wrapped_in_sudo() {
        assert!(matches!(
            base_fee_call::set_base_fee_per_gas(U256::zero()),
            RuntimeCall::Sudo(_)
        ));
        assert!(matches!(
            base_fee_call::set_elasticity(0).unwrap(),
            RuntimeCall::Sudo(_)
        ));
    }

    #[test]
    fn payout_stakers_is_a_staking_call() {
        let encoded = payout_stakers_call::payout_stakers(H160::repeat_byte(1), 7).encode();
        assert!(!encoded.is_empty());
        assert!(matches!(
            payout_stakers_call::payout_stakers(H160::repeat_byte(1), 7),
            RuntimeCall::Staking(_)
        ));
    }
}
