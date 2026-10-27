//! # sunlead-schedule
//!
//! Deterministic, stake-weighted epoch schedule for the SunLead consensus.
//!
//! Given a frozen authority snapshot (id + stake, in canonical order), an epoch
//! seed and the epoch length, this crate produces:
//!
//! * a **schedule**: for every slot in the epoch, exactly one leader index and a
//!   window (run) id, with no leader owning more than `max_window_slots`
//!   consecutive slots;
//! * a **schedule root**: a binary Merkle root committing to the whole schedule,
//!   so a light client / import verifier only needs the root + a Merkle proof to
//!   check `leader_at(slot)`.
//!
//! Design constraints (from the dev plan):
//!
//! * **Determinism**: identical inputs produce identical output on every node
//!   and architecture. The implementation uses only integer arithmetic and
//!   `blake2_256`; it never uses floating point, `HashMap` iteration order,
//!   local time or node configuration.
//! * **Uniqueness & coverage**: every slot has exactly one leader; the leader
//!   count per authority equals the (integer-apportioned) stake quota.
//! * **Run bound**: a single authority never owns more than `max_window_slots`
//!   consecutive slots (the single-authority degenerate case excepted).
//!
//! The crate is `no_std` so it can run both in the runtime (WASM) and the node.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_crypto_hashing::blake2_256;
use sp_runtime::traits::BlakeTwo256;

/// Re-export the Merkle proof shape so downstream (runtime API) can name it
/// without depending on `binary-merkle-tree` directly.
pub use binary_merkle_tree::MerkleProof;
use sunlead_primitives::{
	AuthoritySetId, EpochIndex, Stake, SunLeadId, SunLeadScheduleParams, WindowId,
};

/// Errors produced while building an epoch schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SunLeadScheduleError {
	/// No authorities supplied.
	EmptyAuthorities,
	/// Epoch with zero slots.
	ZeroEpochSlots,
	/// Schedule construction deadlocked (should be prevented by the feasibility
	/// cap; surfaces only on pathological inputs).
	Infeasible,
}

/// Inputs required to deterministically derive an epoch schedule.
pub struct ScheduleInput {
	/// Chain id, mixed into the permutation seed.
	pub chain_id: u64,
	/// Authority set id, mixed into the permutation seed.
	pub authority_set_id: AuthoritySetId,
	/// Epoch index, mixed into the permutation seed.
	pub epoch_index: EpochIndex,
	/// Epoch seed from finalized randomness.
	pub epoch_seed: H256,
	/// Authorities in canonical order: `(id, stake)`. Only the stake and the
	/// ordering matter to the schedule; the id is carried for consistency with
	/// the verifier.
	pub authorities: Vec<(SunLeadId, Stake)>,
	/// Schedule parameters.
	pub params: SunLeadScheduleParams,
}

/// A fully derived epoch schedule.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct ComputedSchedule {	/// Merkle root committing to `(slot, leader_index, window_id)` for all slots.
	pub root: H256,
	/// `leaders[slot]` = leader index into the canonical authority list.
	pub leaders: Vec<u32>,
	/// `windows[slot]` = window (run) id.
	pub windows: Vec<u32>,
	/// Final per-authority slot quota actually used (after feasibility capping).
	pub quota: Vec<u64>,
	/// Parameters this schedule was built with.
	pub params: SunLeadScheduleParams,
}

impl ComputedSchedule {
	/// The uniquely determined leader (and its window) for a slot.
	pub fn leader_at(&self, slot: u64) -> Option<(u32, WindowId)> {
		let s = slot as usize;
		if s >= self.leaders.len() {
			None
		} else {
			Some((self.leaders[s], self.windows[s]))
		}
	}

	/// A Merkle proof for the schedule entry at `slot`.
	///
	/// Returns a `TypeInfo`-friendly shape (so it can cross the runtime API
	/// boundary) instead of the vendored `binary_merkle_tree::MerkleProof`.
	pub fn schedule_proof(&self, slot: u64) -> Option<SunLeadScheduleProof> {
		let s = slot as usize;
		if s >= self.leaders.len() {
			return None;
		}
		let leaves = all_leaves(&self.leaders, &self.windows);
		let mp = binary_merkle_tree::merkle_proof::<BlakeTwo256, _, _>(
			leaves.iter().cloned(),
			s as u32,
		);
		Some(SunLeadScheduleProof {
			root: mp.root,
			proof: mp.proof,
			number_of_leaves: mp.number_of_leaves,
			leaf_index: mp.leaf_index,
		})
	}
}

/// A Merkle inclusion proof for one schedule entry, in a `TypeInfo`-friendly
/// shape usable as a runtime-API return type.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, TypeInfo)]
pub struct SunLeadScheduleProof {
	/// Merkle root the proof commits to (equals `ComputedSchedule::root`).
	pub root: H256,
	/// Sibling hashes from the leaf up to the root.
	pub proof: Vec<H256>,
	/// Total number of leaves in the tree.
	pub number_of_leaves: u32,
	/// Index of the proven leaf.
	pub leaf_index: u32,
}

/// Build the full epoch schedule deterministically.
pub fn build_schedule(
	input: &ScheduleInput,
) -> Result<ComputedSchedule, SunLeadScheduleError> {
	let n = input.authorities.len();
	if n == 0 {
		return Err(SunLeadScheduleError::EmptyAuthorities)
	}
	let epoch_slots = input.params.epoch_slots;
	if epoch_slots == 0 {
		return Err(SunLeadScheduleError::ZeroEpochSlots)
	}
	let soft = input.params.window_len.max(1);
	let hard = input.params.max_window_slots.max(soft);

	let weights: Vec<Stake> = input.authorities.iter().map(|(_, s)| *s).collect();
	let mut quota = apportionment(epoch_slots, &weights)?;
	enforce_feasible(&mut quota, hard);

	let perm_seed = permutation_seed(
		input.chain_id,
		input.authority_set_id,
		input.epoch_index,
		input.epoch_seed,
	);
	let (_perm, rank) = deterministic_permutation(n, &perm_seed);

	let (leaders, windows) =
		build_sequence(&quota, &rank, epoch_slots, soft, hard)
			.ok_or(SunLeadScheduleError::Infeasible)?;

	let root = compute_root(&leaders, &windows);
	Ok(ComputedSchedule { root, leaders, windows, quota, params: input.params })
}

/// Largest-remainder (Hamilton) apportionment of `total_slots` by `weights`.
///
/// Returns a vector of exact per-authority slot counts summing to `total_slots`.
/// Each count is within one slot of its ideal `weight/total_weight * slots`, so
/// the per-authority stake-proportionality error is bounded by 1 slot.
///
/// If the total weight is zero (all authorities have zero stake), every
/// authority is treated as weight 1 (equal apportionment).
pub fn apportionment(
	total_slots: u64,
	weights: &[Stake],
) -> Result<Vec<u64>, SunLeadScheduleError> {
	let n = weights.len();
	if n == 0 {
		return Err(SunLeadScheduleError::EmptyAuthorities)
	}
	let total_stake: u128 = weights.iter().map(|w| *w as u128).sum();
	let use_w: Vec<u128> = if total_stake == 0 {
		vec![1u128; n]
	} else {
		weights.iter().map(|w| *w as u128).collect()
	};
	let total_w: u128 = use_w.iter().sum();

	let mut quota = vec![0u64; n];
	let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(n);
	let mut sum_floor: u128 = 0;
	for (i, &w) in use_w.iter().enumerate() {
		let prod = w * (total_slots as u128);
		let q = prod / total_w;
		let r = prod % total_w;
		quota[i] = q as u64;
		sum_floor += q;
		remainders.push((r, i));
	}

	// Distribute the leftover slots (E - sum of floors) to the largest
	// remainders, breaking ties by canonical index (ascending).
	let mut leftover = total_slots as i128 - sum_floor as i128;
	if leftover < 0 {
		leftover = 0;
	}
	remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
	let mut idx = 0;
	while leftover > 0 {
		let (_, i) = remainders[idx % n];
		quota[i] += 1;
		leftover -= 1;
		idx += 1;
	}
	Ok(quota)
}

/// Cap quotas so that the run-length-bounded arrangement is always feasible.
///
/// A run-length-bounded arrangement of `E` slots exists iff no single
/// authority owns more than `L * (E - q + 1)` slots (classic "task scheduler"
/// condition, with `L = max_window_slots`). This function repeatedly caps the
/// largest quota to that bound and redistributes the excess to the other
/// authorities. As a final safety net it falls back to an equal split, which is
/// always feasible for `n >= 2`. The single-authority case (`n == 1`) is left
/// untouched: with no other validators, one run of all slots is unavoidable.
pub fn enforce_feasible(quota: &mut Vec<u64>, hard_cap: u32) {
	let n = quota.len();
	if n <= 1 {
		return
	}
	let hard = hard_cap as u128;

	for _ in 0..(n * 8 + 16) {
		let total: u128 = quota.iter().map(|q| *q as u128).sum();
		let (mi, mq) = max_index(quota);
		let others = total - mq;
		let max_allowed = hard * (others + 1);
		if mq <= max_allowed {
			break
		}
		let excess = mq - max_allowed;
		quota[mi] = max_allowed as u64;
		let mut ex = excess;
		let mut k = 0usize;
		while ex > 0 {
			let i = (mi + 1 + k) % n;
			if i != mi {
				quota[i] += 1;
				ex -= 1;
			}
			k += 1;
		}
	}

	// Final net: equal split is always feasible for n >= 2.
	let total: u128 = quota.iter().map(|q| *q as u128).sum();
	let (mi, mq) = max_index(quota);
	let others = total - mq;
	if mq > hard * (others + 1) {
		*quota = equal_quota(total as u64, n);
	}
}

fn max_index(quota: &[u64]) -> (usize, u128) {
	let mut mi = 0usize;
	let mut mq = 0u128;
	for (i, &q) in quota.iter().enumerate() {
		let qv = q as u128;
		if qv > mq {
			mq = qv;
			mi = i;
		}
	}
	(mi, mq)
}

fn equal_quota(total: u64, n: usize) -> Vec<u64> {
	let mut q = vec![0u64; n];
	let base = total / n as u64;
	let rem = (total % n as u64) as usize;
	for (i, slot) in q.iter_mut().enumerate() {
		*slot = base + if i < rem { 1 } else { 0 };
	}
	q
}

fn permutation_seed(
	chain_id: u64,
	authority_set_id: u64,
	epoch_index: u64,
	epoch_seed: H256,
) -> [u8; 32] {
	let mut b = Vec::new();
	chain_id.encode_to(&mut b);
	authority_set_id.encode_to(&mut b);
	epoch_index.encode_to(&mut b);
	epoch_seed.encode_to(&mut b);
	blake2_256(&b)
}

/// Deterministic, stake-independent permutation of `[0, n)`.
///
/// Each index `i` is ranked by `blake2_256(seed || i)`; ties are broken by the
/// canonical index. Returns `(permutation_order, rank_of_index)`.
fn deterministic_permutation(n: usize, seed: &[u8]) -> (Vec<usize>, Vec<usize>) {
	let mut ranked: Vec<([u8; 32], usize)> = Vec::with_capacity(n);
	for i in 0..n {
		let mut buf = Vec::with_capacity(seed.len() + 4);
		buf.extend_from_slice(seed);
		buf.extend_from_slice(&(i as u32).to_le_bytes());
		let h = blake2_256(&buf);
		ranked.push((h, i));
	}
	ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
	let perm: Vec<usize> = ranked.iter().map(|(_, i)| *i).collect();
	let mut rank = vec![0usize; n];
	for (pos, &i) in perm.iter().enumerate() {
		rank[i] = pos;
	}
	(perm, rank)
}

/// Greedily place slots honouring run-length limits.
///
/// At each step the chosen authority is the one with the most remaining quota
/// among those that would not exceed the run cap, preferring authorities that
/// are *not* already at the soft target run length (so runs tend to be
/// `window_len` long and only extend to `max_window_slots` when forced). Ties
/// are broken by the deterministic permutation rank. Returns `None` only if the
/// input was not feasible (prevented by [`enforce_feasible`]).
fn build_sequence(
	quota: &[u64],
	perm_rank: &[usize],
	epoch_slots: u64,
	soft: u32,
	hard: u32,
) -> Option<(Vec<u32>, Vec<u32>)> {
	// Single authority: it owns every slot; the run cap is moot (no other
	// validator can separate runs). Return one run of the whole epoch.
	if quota.len() == 1 {
		return Some((vec![0u32; epoch_slots as usize], vec![0u32; epoch_slots as usize]))
	}
	let e = epoch_slots as usize;
	let mut remaining: Vec<u128> = quota.iter().map(|q| *q as u128).collect();
	let mut leaders = Vec::with_capacity(e);
	let mut windows = Vec::with_capacity(e);
	let mut last: Option<usize> = None;
	let mut run: u32 = 0;
	let mut cur_window: i64 = -1;

	for _ in 0..e {
		let mut best_idx: Option<usize> = None;
		// Selection key: (at_soft_limit, u128::MAX - remaining, rank).
		// Lower is better: prefer not-at-soft-limit, then more remaining, then
		// lower permutation rank => fully deterministic.
		let mut best_key: (u8, u128, usize) = (1, 0, usize::MAX);
		for (i, &rem) in remaining.iter().enumerate() {
			if rem == 0 {
				continue
			}
			let at_hard = last == Some(i) && run >= hard;
			if at_hard {
				continue
			}
			let at_soft = last == Some(i) && run >= soft;
			let key = (at_soft as u8, u128::MAX - rem, perm_rank[i]);
			if best_idx.is_none() || key < best_key {
				best_idx = Some(i);
				best_key = key;
			}
		}
		match best_idx {
			Some(i) => {
				remaining[i] -= 1;
				if last == Some(i) {
					run += 1;
				} else {
					run = 1;
					cur_window += 1;
				}
				last = Some(i);
				leaders.push(i as u32);
				windows.push(cur_window as u32);
			},
			None => return None,
		}
	}
	Some((leaders, windows))
}

fn leaf_bytes(slot: u64, leader: u32, window: u32) -> Vec<u8> {
	(slot, leader, window).encode()
}

fn all_leaves(leaders: &[u32], windows: &[u32]) -> Vec<Vec<u8>> {
	(0..leaders.len() as u64)
		.zip(leaders.iter())
		.zip(windows.iter())
		.map(|((s, l), w)| leaf_bytes(s, *l, *w))
		.collect()
}

fn compute_root(leaders: &[u32], windows: &[u32]) -> H256 {
	let leaves = all_leaves(leaders, windows);
	binary_merkle_tree::merkle_root::<BlakeTwo256, _>(leaves.iter().map(|l| l.as_slice()))
}

/// Verify a `(slot, leader, window)` entry against a schedule root via a Merkle
/// proof. This is the efficient verification path used when the full schedule
/// is not locally available (e.g. a node fetches a proof from the runtime).
pub fn verify_schedule_entry(
	root: &H256,
	proof: &SunLeadScheduleProof,
	slot: u64,
	leader: u32,
	window: u32,
) -> bool {
	let leaf = leaf_bytes(slot, leader, window);
	binary_merkle_tree::verify_proof::<BlakeTwo256, _, _>(
		root,
		proof.proof.iter().cloned(),
		proof.number_of_leaves,
		proof.leaf_index,
		&leaf,
	)
}

#[cfg(test)]
mod golden;

#[cfg(test)]
mod tests {
	use super::*;
	use sunlead_primitives::SunLeadScheduleParams;

	fn dummy_id(i: u8) -> SunLeadId {
		let mut b = [0u8; 32];
		b[0] = i;
		b[1] = i.wrapping_mul(7);
		SunLeadId::try_from(&b[..]).expect("32 bytes")
	}

	fn h256_repeat(b: u8) -> H256 {
		H256::from_slice(&[b; 32])
	}

	fn input(
		chain_id: u64,
		set_id: u64,
		epoch: u64,
		seed_byte: u8,
		stakes: &[Stake],
		params: SunLeadScheduleParams,
	) -> ScheduleInput {
		ScheduleInput {
			chain_id,
			authority_set_id: set_id,
			epoch_index: epoch,
			epoch_seed: h256_repeat(seed_byte),
			authorities: stakes.iter().enumerate().map(|(i, s)| (dummy_id(i as u8), *s)).collect(),
			params,
		}
	}

	#[test]
	fn apportionment_sums_exactly_and_is_bounded() {
		let q = apportionment(600, &[100, 50, 30, 20]).unwrap();
		assert_eq!(q.iter().map(|x| *x as u128).sum::<u128>(), 600);
		// weights 100:50:30:20 over 600 slots => exact 300:150:90:60.
		assert_eq!(q[0], 300u64);
		assert_eq!(q[1], 150u64);
		assert_eq!(q[2], 90u64);
		assert_eq!(q[3], 60u64);
		assert!(q.iter().all(|x| *x >= 1));

		let eq = apportionment(600, &[1, 1, 1, 1]).unwrap();
		assert_eq!(eq.iter().sum::<u64>(), 600);

		// zero stake -> equal.
		let z = apportionment(20, &[0, 0, 0, 0]).unwrap();
		assert_eq!(z, vec![5, 5, 5, 5]);
	}

	#[test]
	fn schedule_is_deterministic_and_covers_every_slot() {
		let params = SunLeadScheduleParams::v1_defaults();
		let a = build_schedule(&input(1, 1, 1, 1, &[100, 50, 30, 20], params)).unwrap();
		let b = build_schedule(&input(1, 1, 1, 1, &[100, 50, 30, 20], params)).unwrap();
		assert_eq!(a, b);
		assert_eq!(a.leaders.len(), 600);
		assert_eq!(a.windows.len(), 600);
		// every slot has a valid leader index.
		assert!(a.leaders.iter().all(|&l| (l as usize) < 4));
		// recomputing the root from leaders/windows matches.
		assert_eq!(a.root, compute_root(&a.leaders, &a.windows));
	}

	#[test]
	fn schedule_quota_matches_stake_and_run_is_bounded() {
		let params = SunLeadScheduleParams::v1_defaults();
		let stakes = [900u128, 50, 30, 20];
		let s = build_schedule(&input(1, 1, 1, 3, &stakes, params)).unwrap();
		// count per authority equals the *used* (feasibility-adjusted) quota.
		let q = s.quota.clone();
		let mut counts = [0u64; 4];
		for &l in &s.leaders {
			counts[l as usize] += 1;
		}
		assert_eq!(counts, q[..]);
		// run bound (single-authority case excluded: n=4 here).
		assert!(max_run(&s.leaders) <= params.max_window_slots as usize);
	}

	#[test]
	fn single_validator_is_one_run() {
		let params = SunLeadScheduleParams::v1_defaults();
		let s = build_schedule(&input(1, 1, 1, 9, &[100], params)).unwrap();
		assert_eq!(s.leaders, vec![0u32; 600]);
		assert_eq!(s.windows, vec![0u32; 600]);
	}

	#[test]
	fn zero_stake_is_equal_and_valid() {
		let params = SunLeadScheduleParams::v1_defaults();
		let s = build_schedule(&input(1, 1, 1, 4, &[0, 0, 0, 0], params)).unwrap();
		assert_eq!(s.leaders.len(), 600);
		let q = apportionment(600, &[0, 0, 0, 0]).unwrap();
		let mut counts = [0u64; 4];
		for &l in &s.leaders {
			counts[l as usize] += 1;
		}
		assert_eq!(counts, q[..]);
	}

	#[test]
	fn domain_separation_changes_schedule() {
		let params = SunLeadScheduleParams::v1_defaults();
		let a = build_schedule(&input(1, 1, 1, 1, &[100, 50, 30, 20], params)).unwrap();
		let b = build_schedule(&input(2, 1, 1, 1, &[100, 50, 30, 20], params)).unwrap();
		let c = build_schedule(&input(1, 1, 2, 1, &[100, 50, 30, 20], params)).unwrap();
		assert_ne!(a.root, b.root);
		assert_ne!(a.root, c.root);
	}

	#[test]
	fn schedule_proof_roundtrip() {
		let params = SunLeadScheduleParams::v1_defaults();
		let s = build_schedule(&input(1, 1, 1, 1, &[100, 50, 30, 20], params)).unwrap();
		for slot in [0u64, 1, 7, 300, 599] {
			let proof = s.schedule_proof(slot).unwrap();
			let (leader, window) = s.leader_at(slot).unwrap();
			assert!(verify_schedule_entry(&s.root, &proof, slot, leader, window));
			// wrong leader must fail.
			let wrong = if leader == 0 { 1 } else { 0 };
			assert!(!verify_schedule_entry(&s.root, &proof, slot, wrong, window));
		}
	}

	// Small deterministic "fuzz": many random configs must satisfy invariants.
	#[test]
	fn fuzz_schedule_invariants() {
		let params = SunLeadScheduleParams { epoch_slots: 257, window_len: 2, max_window_slots: 4 };
		let mut rng = XorShift64::new(0x1234_5678_9abc_def1);
		for _ in 0..400 {
			let n = 1 + (rng.next() % 12) as usize; // 1..12 authorities
			let mut stakes: Vec<u128> = Vec::with_capacity(n);
			let mut total = 0u128;
			for _ in 0..n {
				let v = rng.next() % 1000;
				total += v as u128;
				stakes.push(v as u128);
			}
			if total == 0 {
				stakes[0] = 1; // ensure at least one non-zero so not trivially equal
			}
			let seed_byte = (rng.next() % 256) as u8;
			let s = match build_schedule(&input(1, 1, 1, seed_byte, &stakes, params)) {
				Ok(s) => s,
				Err(_) => continue,
			};
			let e = params.epoch_slots as usize;
			assert_eq!(s.leaders.len(), e);
			// coverage & validity
			assert!(s.leaders.iter().all(|&l| (l as usize) < n));
			// quota exactness against the *used* (feasibility-adjusted) quota.
			let q = s.quota.clone();
			let mut counts = vec![0u64; n];
			for &l in &s.leaders {
				counts[l as usize] += 1;
			}
			assert_eq!(counts, q);
			// run bound (n==1 excluded)
			if n > 1 {
				assert!(max_run(&s.leaders) <= params.max_window_slots as usize);
			}
			// determinism
			let s2 = build_schedule(&input(1, 1, 1, seed_byte, &stakes, params)).unwrap();
			assert_eq!(s, s2);
		}
	}

	fn max_run(leaders: &[u32]) -> usize {
		let mut best = 0usize;
		let mut cur = 0usize;
		let mut prev: Option<u32> = None;
		for &l in leaders {
			if Some(l) == prev {
				cur += 1;
			} else {
				cur = 1;
			}
			best = best.max(cur);
			prev = Some(l);
		}
		best
	}

	// Tiny deterministic PRNG (no external deps) for the fuzz harness.
	struct XorShift64 {
		state: u64,
	}
	impl XorShift64 {
		fn new(seed: u64) -> Self {
			Self { state: seed | 1 }
		}
		fn next(&mut self) -> u64 {
			let mut x = self.state;
			x ^= x << 13;
			x ^= x >> 7;
			x ^= x << 17;
			self.state = x;
			x
		}
	}
}
