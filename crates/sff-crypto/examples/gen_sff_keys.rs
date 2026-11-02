//! Prints the BLS public key + proof-of-possession that the SFF shadow signer
//! (`DevSeedSigner` in `node/src/fast_finality.rs`) derives for a given dev seed
//! string. The seed-derivation here MUST match `node/src/service.rs` exactly:
//! raw bytes of the string, left-aligned into a 32-byte array (no hashing).
//!
//! Usage: `cargo run -p sff-crypto --example gen_sff_keys -- "//Alice" "//Bob"`

use sff_crypto::{pop_sign, BlsKeypair};
use sff_primitives::{BlsPublicKey, BlsSignature};

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(seed_str: &str) -> (BlsPublicKey, BlsSignature) {
	let b = seed_str.as_bytes();
	let mut seed = [0u8; 32];
	let n = b.len().min(32);
	seed[..n].copy_from_slice(&b[..n]);
	let kp = BlsKeypair::from_seed(&seed);
	let pk = kp.public();
	let pop = pop_sign(&kp);
	(pk, pop)
}

fn main() {
	for seed_str in std::env::args().skip(1) {
		let (pk, pop) = derive(&seed_str);
		println!("seed={seed_str} pk={} pop={}", hex(&pk.0), hex(&pop.0));
	}
}
