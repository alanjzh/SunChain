use anyhow::Result;
use sunchain_runtime::{RuntimeCall, SignedPayload, TxExtension, UncheckedExtrinsic};
use frame_metadata_hash_extension::CheckMetadataHash;
use frame_system::{
    CheckEra, CheckGenesis, CheckNonZeroSender, CheckNonce, CheckSpecVersion, CheckTxVersion,
    CheckWeight,
};
use pallet_transaction_payment::ChargeTransactionPayment;
use scale_codec::Encode;
use sp_runtime::generic::Era;

use crate::{rpc::RuntimeInfo, signer::EthereumSigner};

pub fn build_signed_extrinsic(
    call: RuntimeCall,
    signer: EthereumSigner,
    info: &RuntimeInfo,
    tip: u128,
    mortality: u64,
) -> Result<UncheckedExtrinsic> {
    let era = if mortality == 0 {
        Era::immortal()
    } else {
        Era::mortal(mortality, info.block_number)
    };
    let extension: TxExtension = (
        CheckNonZeroSender::new(),
        CheckSpecVersion::new(),
        CheckTxVersion::new(),
        CheckGenesis::new(),
        CheckEra::from(era),
        CheckNonce::from(info.nonce),
        CheckWeight::new(),
        ChargeTransactionPayment::from(tip),
        CheckMetadataHash::new(false),
        frame_system::WeightReclaim::new(),
    );
    let payload = SignedPayload::from_raw(
        call.clone(),
        extension.clone(),
        (
            (),
            info.spec_version,
            info.transaction_version,
            info.genesis_hash,
            info.block_hash,
            (),
            (),
            (),
            None,
            (),
        ),
    );
    let signature = payload.using_encoded(|bytes| signer.sign(bytes));
    Ok(UncheckedExtrinsic::new_signed(
        call,
        signer.account_id(),
        signature,
        extension,
    ))
}
