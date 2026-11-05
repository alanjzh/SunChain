//! # sff-primitives
//!
//! Shared, `no_std`-compatible data types for the **Sunchain Fast Finality (SFF)**
//! protocol described in `docs/Consensus/Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md`.
//!
//! This crate contains **only** pure data types, SCALE encodings, the domain
//! separation scheme, the authority snapshot shape and the shared error codes.
//! It performs **no** BLS math, no networking, no fork-choice and no runtime
//! lookups, so it can be compiled into the runtime WASM blob and reused by the
//! node-side verifier/aggregator.
//!
//! F1 (this crate + `sff-crypto` + `sff-verifier`) is pure logic and
//! cryptography; nothing here is wired into BABE, GRANDPA or the block import
//! pipeline. SFF is a shadow/fast-confirmation signal only and never an
//! authoritative finality writer (see design §1, §4.6).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_crypto_hashing::blake2_256;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Current SFF protocol version. Bumped on any wire/certificate-format change.
pub const SFF_PROTOCOL_VERSION: u16 = 1;

/// Compressed BLS12-381 G1 element length (signatures / PoP).
pub const BLS_G1_COMPRESSED_LEN: usize = 48;
/// Compressed BLS12-381 G2 element length (public keys).
pub const BLS_G2_COMPRESSED_LEN: usize = 96;

/// Hard cap on the number of authorities in a single [`FinalityAuthoritySet`].
///
/// Bounds every bitmap, stake sum and aggregation loop so a malicious
/// aggregator cannot force unbounded allocation or iteration (design §7).
pub const MAX_AUTHORITIES: usize = 1024;
/// Maximum bitmap byte length derived from [`MAX_AUTHORITIES`].
pub const MAX_BITMAP_BYTES: usize = MAX_AUTHORITIES.div_ceil(8);

/// Domain-separation tag for a [`Vote`] / [`QuorumCertificate`] message.
///
/// Prefixes every signed payload so a certificate cannot be replayed across
/// chain / session / version (design §4.2, §4.3).
pub const DOMAIN_VOTE: &[u8; 12] = b"SUN_SFF_VOTE";
/// Domain-separation tag for a proof-of-possession signature over a BLS pubkey.
pub const DOMAIN_POP: &[u8; 11] = b"SUN_SFF_POP";

// ---------------------------------------------------------------------------
// BLS byte wrappers
// ---------------------------------------------------------------------------

/// A BLS12-381 **signature** (min-sig variant: G1, 48 compressed bytes).
///
/// Stored as raw bytes so it SCALE-encodes identically inside `no_std` and
/// `std`. Cryptographic validation happens in `sff-crypto`.
#[derive(Clone, Copy, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct BlsSignature(pub [u8; BLS_G1_COMPRESSED_LEN]);

impl core::fmt::Debug for BlsSignature {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "BlsSignature(0x")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "..)")
    }
}

impl BlsSignature {
    /// The all-zero (identity / invalid) signature.
    pub const ZERO: Self = BlsSignature([0u8; BLS_G1_COMPRESSED_LEN]);
    /// Returns the raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// Builds from raw bytes, failing if the length is wrong.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != BLS_G1_COMPRESSED_LEN {
            return None;
        }
        let mut a = [0u8; BLS_G1_COMPRESSED_LEN];
        a.copy_from_slice(b);
        Some(BlsSignature(a))
    }
    /// True if this is the identity/zero signature (never a valid aggregate).
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
}

impl MaxEncodedLen for BlsSignature {
    fn max_encoded_len() -> usize {
        BLS_G1_COMPRESSED_LEN
    }
}

impl Default for BlsSignature {
    fn default() -> Self {
        BlsSignature([0u8; BLS_G1_COMPRESSED_LEN])
    }
}

/// A BLS12-381 **public key** (min-sig variant: G2, 96 compressed bytes).
#[derive(Clone, Copy, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct BlsPublicKey(pub [u8; BLS_G2_COMPRESSED_LEN]);

impl core::fmt::Debug for BlsPublicKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "BlsPublicKey(0x")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "..)")
    }
}

impl BlsPublicKey {
    /// The all-zero (identity) public key.
    pub const ZERO: Self = BlsPublicKey([0u8; BLS_G2_COMPRESSED_LEN]);
    /// Returns the raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// Builds from raw bytes, failing if the length is wrong.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != BLS_G2_COMPRESSED_LEN {
            return None;
        }
        let mut a = [0u8; BLS_G2_COMPRESSED_LEN];
        a.copy_from_slice(b);
        Some(BlsPublicKey(a))
    }
    /// True if this is the identity/zero key (never a valid participant).
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
}

impl MaxEncodedLen for BlsPublicKey {
    fn max_encoded_len() -> usize {
        BLS_G2_COMPRESSED_LEN
    }
}

impl Default for BlsPublicKey {
    fn default() -> Self {
        BlsPublicKey([0u8; BLS_G2_COMPRESSED_LEN])
    }
}

// ---------------------------------------------------------------------------
// Vote phase
// ---------------------------------------------------------------------------

/// The phase a [`Vote`] or [`QuorumCertificate`] attests to.
///
/// Two-phase locking (design §4.4): `Prepare` records that a validator
/// executed a candidate block; `Commit` records finality intent and is only
/// signed once a valid `PrepareQC` for the same block is observed. `Skip`
/// advances the round when no `PrepareQC` formed in time.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Encode,
    Decode,
    DecodeWithMemTracking,
    TypeInfo,
    MaxEncodedLen,
)]
#[repr(u8)]
pub enum Phase {
    /// Attests execution of a candidate block (no finality yet).
    Prepare = 0,
    /// Attests finality intent, gated on a valid PrepareQC.
    Commit = 1,
    /// Advances the round; never covers a conflicting committed branch.
    Skip = 2,
}

impl Phase {
    /// Maps the discriminant back to a [`Phase`], or `None` if unknown.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Phase::Prepare),
            1 => Some(Phase::Commit),
            2 => Some(Phase::Skip),
            _ => None,
        }
    }

    /// Returns the `block_hash`-dependent invariant for this phase.
    ///
    /// `Prepare`/`Commit` require a concrete block; `Skip` requires `None`.
    pub fn requires_block(&self) -> bool {
        matches!(self, Phase::Prepare | Phase::Commit)
    }
}

// ---------------------------------------------------------------------------
// Canonical message bytes (domain-separated, unambiguous)
// ---------------------------------------------------------------------------

/// Builds the canonical signed message bytes for a vote/certificate.
///
/// The signature is **not** part of these bytes (it attests them). Every
/// domain-separating, anti-replay field is included: protocol version,
/// `genesis_hash`, `set_id`, `snapshot_root`, height, round, phase, the target
/// block, its parent, and (for `Commit`) the PrepareQC hash.
#[cfg_attr(not(feature = "std"), allow(unused_mut))]
pub fn canonical_message_bytes(
    version: u16,
    genesis_hash: &H256,
    set_id: u64,
    snapshot_root: &H256,
    height: u32,
    round: u32,
    phase: Phase,
    block_hash: Option<&H256>,
    parent_hash: &H256,
    prepare_qc_hash: Option<&H256>,
) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(DOMAIN_VOTE);
    v.extend_from_slice(&version.to_le_bytes());
    v.extend_from_slice(genesis_hash.as_bytes());
    v.extend_from_slice(&set_id.to_le_bytes());
    v.extend_from_slice(snapshot_root.as_bytes());
    v.extend_from_slice(&height.to_le_bytes());
    v.extend_from_slice(&round.to_le_bytes());
    v.push(phase as u8);
    match block_hash {
        Some(h) => {
            v.push(1);
            v.extend_from_slice(h.as_bytes());
        }
        None => v.push(0),
    }
    v.extend_from_slice(parent_hash.as_bytes());
    match prepare_qc_hash {
        Some(h) => {
            v.push(1);
            v.extend_from_slice(h.as_bytes());
        }
        None => v.push(0),
    }
    v
}

/// Blake2-256 hash of the canonical message bytes.
pub fn message_hash(bytes: &[u8]) -> H256 {
    H256::from_slice(&blake2_256(bytes))
}

/// Canonical PoP message bytes: domain tag followed by the public-key bytes.
pub fn pop_message_bytes(pk: &BlsPublicKey) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(DOMAIN_POP);
    v.extend_from_slice(&pk.0);
    v
}

// ---------------------------------------------------------------------------
// Signer bitmap
// ---------------------------------------------------------------------------

/// A bounded, SCALE-encoded bitmap of which authorities signed a certificate.
///
/// Bit `i` set means authority `i` of the referenced [`FinalityAuthoritySet`]
/// contributed its signature. Length is bounded by [`MAX_BITMAP_BYTES`], and a
/// bitmap can never encode a duplicate signer — the verifier independently
/// recomputes the signed stake from set bits (design §4.3, §7).
#[derive(Clone, PartialEq, Eq, Default, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct SignerBitmap(pub Vec<u8>);

impl core::fmt::Debug for SignerBitmap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SignerBitmap(len_bits={})", self.len_bits())
    }
}

impl SignerBitmap {
    /// Creates a zeroed bitmap large enough for `n` authorities.
    pub fn with_capacity(n: usize) -> Self {
        let bytes = n.div_ceil(8);
        SignerBitmap(vec![0u8; bytes])
    }

    /// Number of representable bits (= 8 * byte length).
    pub fn len_bits(&self) -> usize {
        self.0.len() * 8
    }

    /// Returns `true` if the bitmap can represent at least `n` authorities.
    pub fn covers(&self, n: usize) -> bool {
        self.len_bits() >= n
    }

    /// Reads bit `i`. Out-of-range reads return `false` (never panic).
    pub fn get(&self, i: usize) -> bool {
        let byte = i / 8;
        let bit = i % 8;
        match self.0.get(byte) {
            Some(b) => (b >> bit) & 1 == 1,
            None => false,
        }
    }

    /// Sets bit `i` to 1. Out-of-range sets are ignored (never panic).
    pub fn set(&mut self, i: usize) {
        let byte = i / 8;
        let bit = i % 8;
        if let Some(b) = self.0.get_mut(byte) {
            *b |= 1u8 << bit;
        }
    }

    /// Number of set bits.
    pub fn count_ones(&self) -> u32 {
        self.0.iter().map(|b| b.count_ones()).sum()
    }

    /// True if at most `max_bytes` are used (bounds check for the verifier).
    pub fn within_bounds(&self) -> bool {
        self.0.len() <= MAX_BITMAP_BYTES
    }

    /// Bitmap of bits set in *both* `self` and `other`.
    pub fn intersection(&self, other: &SignerBitmap) -> SignerBitmap {
        let len = self.0.len().max(other.0.len());
        let mut out = vec![0u8; len];
        for i in 0..len {
            let sb = *self.0.get(i).unwrap_or(&0);
            let ob = *other.0.get(i).unwrap_or(&0);
            out[i] = sb & ob;
        }
        SignerBitmap(out)
    }
}

impl MaxEncodedLen for SignerBitmap {
    fn max_encoded_len() -> usize {
        MAX_BITMAP_BYTES + codec::Compact::<u32>::max_encoded_len()
    }
}

// ---------------------------------------------------------------------------
// Vote & QuorumCertificate
// ---------------------------------------------------------------------------

/// A single validator's signed vote in SFF.
///
/// `signer_index` identifies the authority within the referenced
/// [`FinalityAuthoritySet`]; it is what makes double-signing attributable for
/// equivocation evidence (design §4.4, §7).
#[derive(Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct Vote {
    /// SFF protocol version.
    pub version: u16,
    /// Genesis hash, binds the vote to this chain.
    pub genesis_hash: H256,
    /// Authority set id the vote belongs to.
    pub set_id: u64,
    /// Snapshot root of the authority set (binds to this exact membership).
    pub snapshot_root: H256,
    /// Block height being attested.
    pub height: u32,
    /// Voting round (used for skip / re-proposal progress).
    pub round: u32,
    /// Attestation phase.
    pub phase: Phase,
    /// Target block hash (`None` for `Skip`).
    pub block_hash: Option<H256>,
    /// Parent hash of the target block (locking-rule anchor).
    pub parent_hash: H256,
    /// PrepareQC hash this Commit is gated on (`Some` only for `Commit`).
    pub prepare_qc_hash: Option<H256>,
    /// Index of the signing authority in the set.
    pub signer_index: u32,
    /// The BLS signature over [`Vote::canonical_bytes`].
    pub signature: BlsSignature,
}

impl Vote {
    /// Canonical signed bytes (the exact message the signature covers).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_message_bytes(
            self.version,
            &self.genesis_hash,
            self.set_id,
            &self.snapshot_root,
            self.height,
            self.round,
            self.phase,
            self.block_hash.as_ref(),
            &self.parent_hash,
            self.prepare_qc_hash.as_ref(),
        )
    }

    /// Hash of the canonical signed bytes.
    pub fn payload_hash(&self) -> H256 {
        message_hash(&self.canonical_bytes())
    }
}

/// An aggregated, threshold-certified attestation.
///
/// The aggregate BLS signature covers the *same* canonical message for every
/// signer, so the verifier reconstructs that message from the QC's own fields
/// and must independently recompute `signed_stake` from the bitmap rather than
/// trusting the provided value (design §4.3).
#[derive(Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct QuorumCertificate {
    /// SFF protocol version.
    pub version: u16,
    /// Genesis hash.
    pub genesis_hash: H256,
    /// Authority set id.
    pub set_id: u64,
    /// Snapshot root of the certifying set.
    pub snapshot_root: H256,
    /// Block height attested.
    pub height: u32,
    /// Voting round.
    pub round: u32,
    /// Phase certified.
    pub phase: Phase,
    /// Target block hash (`None` for `Skip`).
    pub block_hash: Option<H256>,
    /// Parent hash of the target block.
    pub parent_hash: H256,
    /// PrepareQC hash a `Commit` QC is gated on.
    pub prepare_qc_hash: Option<H256>,
    /// Hash of the canonical message the aggregate covers (consistency check).
    pub vote_payload_hash: H256,
    /// Aggregated BLS signature over the canonical message.
    pub aggregate_signature: BlsSignature,
    /// Bitmap of contributing signers.
    pub signer_bitmap: SignerBitmap,
    /// Total stake of the signers (verifier recomputes this independently).
    pub signed_stake: u128,
}

impl QuorumCertificate {
    /// Reconstructs the canonical message this QC's aggregate signature covers.
    pub fn canonical_message(&self) -> Vec<u8> {
        canonical_message_bytes(
            self.version,
            &self.genesis_hash,
            self.set_id,
            &self.snapshot_root,
            self.height,
            self.round,
            self.phase,
            self.block_hash.as_ref(),
            &self.parent_hash,
            self.prepare_qc_hash.as_ref(),
        )
    }

    /// Hash of the reconstructed canonical message.
    pub fn payload_hash(&self) -> H256 {
        message_hash(&self.canonical_message())
    }
}

// ---------------------------------------------------------------------------
// Authority snapshot
// ---------------------------------------------------------------------------

/// One member of a [`FinalityAuthoritySet`].
#[derive(Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct FinalityAuthority {
    /// Native validator id (e.g. stash/controller account, opaque here).
    pub validator_id: H256,
    /// GRANDPA authority id, kept for provenance / cross-checks.
    pub grandpa_id: H256,
    /// BLS12-381 public key (G2, min-sig).
    pub bls_public_key: BlsPublicKey,
    /// Proof-of-possession BLS signature over [`pop_message_bytes`].
    pub proof_of_possession: BlsSignature,
    /// Frozen stake weight used for threshold math.
    pub stake: u128,
}

/// Immutable, epoch-frozen set of finality authorities (design §4.2).
///
/// `stake` and membership are taken from the runtime's frozen session snapshot;
/// they are never inferred from a node-local cache. `snapshot_root` binds the
/// certificate to this exact membership and is part of every signature.
#[derive(Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct FinalityAuthoritySet {
    /// Authority set id (mirrors GRANDPA set id).
    pub set_id: u64,
    /// Block at which this set activates.
    pub activation_block: u32,
    /// Sum of all authorities' `stake`.
    pub total_stake: u128,
    /// Authority list (index = signer index in bitmaps/votes).
    pub authorities: Vec<FinalityAuthority>,
    /// Merkle-ish root binding the membership (excludes itself).
    pub snapshot_root: H256,
}

impl FinalityAuthoritySet {
    /// Builds a set, computing `total_stake` and `snapshot_root` from the
    /// authorities. Returns `None` if the authority count exceeds
    /// [`MAX_AUTHORITIES`].
    pub fn new(
        set_id: u64,
        activation_block: u32,
        authorities: Vec<FinalityAuthority>,
    ) -> Option<Self> {
        if authorities.len() > MAX_AUTHORITIES {
            return None;
        }
        let total_stake = authorities
            .iter()
            .fold(0u128, |acc, a| acc.saturating_add(a.stake));
        let root = Self::root_of(&authorities);
        Some(FinalityAuthoritySet {
            set_id,
            activation_block,
            total_stake,
            authorities,
            snapshot_root: root,
        })
    }

    /// Computes the snapshot root over the authorities (excludes self field).
    pub fn root_of(authorities: &[FinalityAuthority]) -> H256 {
        message_hash(&authorities.encode())
    }

    /// Number of authorities.
    pub fn len(&self) -> usize {
        self.authorities.len()
    }

    /// True if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.authorities.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Verification / construction errors shared by `sff-verifier` and
/// `sff-crypto` (design §7 lists the properties each guards).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SffError {
    /// `set_id` in the message/certificate does not match the authority set.
    SetIdMismatch,
    /// `snapshot_root` does not match the authority set (membership replay).
    SnapshotRootMismatch,
    /// Bitmap is shorter than the number of authorities (cannot address all).
    BitmapTooShort,
    /// A set bit indexes beyond the authority list.
    BitmapIndexOutOfRange,
    /// Bitmap byte length exceeds [`MAX_BITMAP_BYTES`].
    BitmapTooLarge,
    /// Signed stake does not strictly exceed 2/3 of `total_stake`.
    InsufficientStake,
    /// Stake summation overflowed `u128` (malformed set).
    StakeOverflow,
    /// `vote_payload_hash` disagrees with the reconstructed message.
    PayloadHashMismatch,
    /// `block_hash` is absent/present contrary to the phase invariant.
    PhaseBlockMismatch,
    /// `prepare_qc_hash` absent for a `Commit` phase.
    MissingPrepareQc,
    /// `Commit` not gated on a valid `PrepareQC` (lock rule violated).
    LockRuleViolation,
    /// Two conflicting certificates at the same height (safety breach).
    ConflictingCommit,
    /// Duplicate signer detected in an aggregator input (should be impossible
    /// via bitmap, but guarded for aggregator safety).
    DuplicateSigner,
    /// Aggregate BLS signature failed to verify.
    InvalidSignature,
    /// BLS public key / signature could not be decoded or is not in subgroup.
    InvalidKey,
    /// Proof-of-possession failed to verify.
    InvalidPop,
    /// Unsupported protocol `version`.
    UnsupportedVersion,
    /// Empty certificate / no signers.
    EmptyCertificate,
}

impl core::fmt::Display for SffError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            SffError::SetIdMismatch => "set_id mismatch",
            SffError::SnapshotRootMismatch => "snapshot_root mismatch",
            SffError::BitmapTooShort => "bitmap too short",
            SffError::BitmapIndexOutOfRange => "bitmap index out of range",
            SffError::BitmapTooLarge => "bitmap too large",
            SffError::InsufficientStake => "insufficient signed stake",
            SffError::StakeOverflow => "stake overflow",
            SffError::PayloadHashMismatch => "payload hash mismatch",
            SffError::PhaseBlockMismatch => "phase/block mismatch",
            SffError::MissingPrepareQc => "missing prepare_qc_hash",
            SffError::LockRuleViolation => "lock rule violation",
            SffError::ConflictingCommit => "conflicting commit",
            SffError::DuplicateSigner => "duplicate signer",
            SffError::InvalidSignature => "invalid signature",
            SffError::InvalidKey => "invalid key",
            SffError::InvalidPop => "invalid proof of possession",
            SffError::UnsupportedVersion => "unsupported version",
            SffError::EmptyCertificate => "empty certificate",
        };
        write!(f, "SffError::{s}")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SffError {}

// ---------------------------------------------------------------------------
// Threshold helper (no overflow, exact > 2/3 test)
// ---------------------------------------------------------------------------

/// Returns `true` iff `signed * 3 > total * 2` (strictly more than 2/3),
/// computed in 256-bit precision so `u128` inputs never overflow.
pub fn exceeds_two_thirds(signed: u128, total: u128) -> bool {
    Wide::mul3(signed) > Wide::mul2(total)
}

/// Returns `true` iff `signed * 3 >= total * 2` (>= 2/3), 256-bit precise.
pub fn at_least_two_thirds(signed: u128, total: u128) -> bool {
    Wide::mul3(signed) >= Wide::mul2(total)
}

/// A 256-bit unsigned integer stored as `(hi, lo)` for overflow-free threshold
/// comparison.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Wide(u128, u128);

impl Wide {
    #[inline]
    fn mul3(x: u128) -> Self {
        let lo = (x & u64::MAX as u128) * 3;
        let hi = (x >> 64) * 3 + (lo >> 64);
        Wide(hi, lo & u64::MAX as u128)
    }

    #[inline]
    fn mul2(x: u128) -> Self {
        let lo = (x & u64::MAX as u128) * 2;
        let hi = (x >> 64) * 2 + (lo >> 64);
        Wide(hi, lo & u64::MAX as u128)
    }
}

impl PartialOrd for Wide {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Wide {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// Sum of the stakes of authorities whose bitmap bit is set.
///
/// Returns `Err(SffError::StakeOverflow)` on saturation and `Err` on any
/// index/length problem so callers cannot accidentally trust a partial sum.
pub fn signed_stake_of(
    set: &FinalityAuthoritySet,
    bitmap: &SignerBitmap,
) -> Result<u128, SffError> {
    if !bitmap.within_bounds() {
        return Err(SffError::BitmapTooLarge);
    }
    if !bitmap.covers(set.authorities.len()) {
        return Err(SffError::BitmapTooShort);
    }
    let mut total = 0u128;
    for (i, auth) in set.authorities.iter().enumerate() {
        if bitmap.get(i) {
            total = total
                .checked_add(auth.stake)
                .ok_or(SffError::StakeOverflow)?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_math_exact() {
        // total = 3: 2/3 boundary. signed=2 -> 6 > 6 false. signed=3 -> 9>6 true.
        assert!(!exceeds_two_thirds(2, 3));
        assert!(exceeds_two_thirds(3, 3));
        // total = 4: 2/3*4 = 2.66 -> need signed>=3. 3*3=9 > 8 true.
        assert!(!exceeds_two_thirds(2, 4));
        assert!(exceeds_two_thirds(3, 4));
        // total = 100: need > 66.66 -> 67. 67*3=201 > 200 true.
        assert!(!exceeds_two_thirds(66, 100));
        assert!(exceeds_two_thirds(67, 100));
        // overflow guard: huge values
        assert!(exceeds_two_thirds(u128::MAX, u128::MAX));
        assert!(!exceeds_two_thirds(u128::MAX / 2, u128::MAX));
    }

    #[test]
    fn bitmap_roundtrip() {
        let mut bm = SignerBitmap::with_capacity(10);
        assert_eq!(bm.len_bits(), 16);
        bm.set(0);
        bm.set(3);
        bm.set(9);
        assert!(bm.get(0) && bm.get(3) && bm.get(9));
        assert!(!bm.get(1));
        assert_eq!(bm.count_ones(), 3);
        let enc = bm.encode();
        let bm2 = SignerBitmap::decode(&mut &enc[..]).unwrap();
        assert_eq!(bm, bm2);
    }

    #[test]
    fn canonical_bytes_are_domain_separated() {
        let g = H256::repeat_byte(1);
        let s = H256::repeat_byte(2);
        let p = H256::repeat_byte(3);
        let b = H256::repeat_byte(4);
        let a = canonical_message_bytes(1, &g, 7, &s, 10, 1, Phase::Prepare, Some(&b), &p, None);
        let c = canonical_message_bytes(1, &g, 7, &s, 10, 1, Phase::Prepare, Some(&b), &p, None);
        assert_eq!(a, c);
        // differing set_id changes bytes
        let d = canonical_message_bytes(1, &g, 8, &s, 10, 1, Phase::Prepare, Some(&b), &p, None);
        assert_ne!(a, d);
        // differing version changes bytes (anti-replay)
        let e = canonical_message_bytes(2, &g, 7, &s, 10, 1, Phase::Prepare, Some(&b), &p, None);
        assert_ne!(a, e);
    }

    #[test]
    fn snapshot_root_is_membership_binding() {
        let a1 = FinalityAuthority {
            validator_id: H256::repeat_byte(1),
            grandpa_id: H256::repeat_byte(2),
            bls_public_key: BlsPublicKey::ZERO,
            proof_of_possession: BlsSignature::ZERO,
            stake: 100,
        };
        let a2 = FinalityAuthority {
            validator_id: H256::repeat_byte(9),
            grandpa_id: H256::repeat_byte(2),
            bls_public_key: BlsPublicKey::ZERO,
            proof_of_possession: BlsSignature::ZERO,
            stake: 100,
        };
        let s1 = FinalityAuthoritySet::new(1, 0, vec![a1.clone()]).unwrap();
        let s2 = FinalityAuthoritySet::new(1, 0, vec![a2]).unwrap();
        assert_ne!(s1.snapshot_root, s2.snapshot_root);
        assert_eq!(s1.total_stake, 100);
        assert!(FinalityAuthoritySet::new(1, 0, vec![a1; MAX_AUTHORITIES + 1]).is_none());
    }
}
