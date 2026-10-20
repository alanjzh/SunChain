use anyhow::Result;
use sunchain_runtime::AccountId;
use fp_account::EthereumSignature;
use sp_core::Pair;
use sp_core::{ecdsa, H160};
use sp_io::hashing::keccak_256;

pub struct EthereumSigner {
    pair: ecdsa::Pair,
    account_id: AccountId,
}

impl EthereumSigner {
    pub fn from_uri(uri: &str) -> Result<Self> {
        let pair = ecdsa::Pair::from_string(uri, None)
            .map_err(|e| anyhow::anyhow!("invalid Ethereum private key/URI: {e:?}"))?;
        let account_id: AccountId = pair.public().into();
        Ok(Self { pair, account_id })
    }

    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub fn sign(&self, payload: &[u8]) -> EthereumSignature {
        let digest = keccak_256(payload);
        EthereumSignature::new(self.pair.sign_prehashed(&digest))
    }
}

pub fn parse_account(value: &str) -> Result<H160> {
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid Ethereum address {value:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_runtime::traits::Verify;

    #[test]
    fn mnemonic_derived_private_key_matches_genesis_address() {
        let signer = EthereumSigner::from_uri(
            "0x2824110040e38dc69fb9f7f90a79c2a379a390cdd8424a1793f9500fa777c378",
        )
        .unwrap();
        assert_eq!(
            format!("{}", signer.account_id()),
            "0x6dE2B57754235cA8f4f7464CF829Ba0BF1456B66"
        );

        let message = b"sun-admin signature test";
        let signature = signer.sign(message);
        assert!(signature.verify(&message[..], &signer.account_id()));
    }
}
