//! Prints the BLS public key + PoP for the **keystore derivation** used by
//! `Keystore::sff_generate_new(Some(seed))`:
//!
//!     BlsKeypair::from_seed(blake2_256(seed))
//!
//! (see `sc-keystore/src/local.rs`; `sff_effective_seed` is the identity when
//! the keystore has no password). This is what `chain_spec::dev_sffb_for` must
//! mirror, and it should equal the pubkey the node logs as
//! "injected dev sffb session key".
//!
//! Usage: `cargo run -q -p sff-crypto --example gen_sff_keys_blake -- "//Alice" "//Bob"`

use sff_crypto::{pop_sign, BlsKeypair};
use sff_primitives::{BlsPublicKey, BlsSignature};
use sp_core::hashing::blake2_256;

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(seed_str: &str) -> (BlsPublicKey, BlsSignature) {
	let seed = blake2_256(seed_str.as_bytes());
	let kp = BlsKeypair::from_seed(&seed);
	(kp.public(), pop_sign(&kp))
}

fn main() {
	for seed_str in std::env::args().skip(1) {
		let (pk, pop) = derive(&seed_str);
		println!("seed={seed_str} pk={} pop={}", hex(&pk.0), hex(&pop.0));
	}
}
