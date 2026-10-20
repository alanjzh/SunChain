//! Who is allowed to publish index prices — one definition, used on both sides of the wire.
//!
//! The oracle feed signs its votes with [`source_keys`]; genesis authorises
//! [`source_accounts`], those same keys' accounts. Nothing checks that the two agree. When
//! they drift apart nothing errors either: every vote is rejected, no index price ever forms,
//! every mark price therefore reads as stale, and every order gets refused inside a batch
//! that still dispatches `Ok` — so the whole DEX stops without a single line saying why.
//!
//! Keeping one list is the only defence: it is the same reason `chain_spec::dex_genesis`
//! exists, but here the failure is even harder to see because it starts several layers below
//! what anybody would think to check.

use sunchain_runtime::AccountId;
use ecdsa_keyring::Keyring;

/// The keys this node's oracle feed signs price votes with.
pub fn source_keys() -> Vec<Keyring> {
	// Two, because the oracle needs `MinSources` of them to agree before a price exists.
	vec![Keyring::Dorothy, Keyring::Ethan]
}

/// Those keys' accounts: the set genesis must authorise.
pub fn source_accounts() -> Vec<AccountId> {
	source_keys().into_iter().map(|key| key.to_account_id()).collect()
}
