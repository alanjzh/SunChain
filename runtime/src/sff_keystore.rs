//! Host bridge for SFF session-key persistence in the node keystore.
use alloc::vec::Vec;
use sp_externalities::ExternalitiesExt;
use sp_keystore::KeystoreExt;
use sp_runtime_interface::pass_by::{
    AllocateAndReturnByCodec, PassFatPointerAndDecode, PassFatPointerAndRead,
};

#[sp_runtime_interface::runtime_interface]
pub trait SffKeystore {
    fn generate(
        &mut self,
        id: u32,
        seed: PassFatPointerAndDecode<Option<Vec<u8>>>,
    ) -> AllocateAndReturnByCodec<Vec<u8>> {
        let id = sp_core::crypto::KeyTypeId(id.to_le_bytes());
        self.extension::<KeystoreExt>()
            .expect("keystore extension required for SFF key generation")
            .sff_generate_new(id, seed.as_deref())
            .expect("failed to generate and persist SFF session key")
            .into()
    }

    fn keys(&mut self, id: u32) -> AllocateAndReturnByCodec<Vec<Vec<u8>>> {
        let id = sp_core::crypto::KeyTypeId(id.to_le_bytes());
        self.extension::<KeystoreExt>()
            .expect("keystore extension required for session key lookup")
            .keys(id)
            .unwrap_or_default()
            .into()
    }

    fn sign(
        &mut self,
        id: u32,
        public: PassFatPointerAndRead<&[u8]>,
        msg: PassFatPointerAndRead<&[u8]>,
    ) -> AllocateAndReturnByCodec<Option<Vec<u8>>> {
        let id = sp_core::crypto::KeyTypeId(id.to_le_bytes());
        self.extension::<KeystoreExt>()
            .expect("keystore extension required for SFF signing")
            .sff_sign(id, public, msg)
            .ok()
            .flatten()
            .into()
    }

    fn generate_pop(
        &mut self,
        id: u32,
        public: PassFatPointerAndRead<&[u8]>,
    ) -> AllocateAndReturnByCodec<Option<Vec<u8>>> {
        let id = sp_core::crypto::KeyTypeId(id.to_le_bytes());
        self.extension::<KeystoreExt>()
            .expect("keystore extension required for SFF PoP")
            .sff_generate_proof_of_possession(id, public)
            .ok()
            .flatten()
            .into()
    }
}
