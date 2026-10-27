//! # Golden vectors
//!
//! A set of fixed scenarios whose derived schedule (root + full leader/window
//! lists) is serialized to JSON. The same JSON is consumed by the standalone
//! and WASM reference implementations so all three agree on the canonical
//! schedule for identical inputs (the dev plan's "共用的 golden vectors").
//!
//! Running `cargo test -p sunlead-schedule` bootstraps `golden/sunlead.golden.json`
//! on first run and verifies it on subsequent runs.

use crate::{build_schedule, ScheduleInput};
use serde::{Deserialize, Serialize};
use sp_core::H256;
use sunlead_primitives::{Stake, SunLeadId, SunLeadScheduleParams};

/// One fixed scenario and its canonical derived schedule.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GoldenScenario {
	pub name: String,
	pub chain_id: u64,
	pub authority_set_id: u64,
	pub epoch_index: u64,
	pub epoch_seed: String,
	pub epoch_slots: u64,
	pub window_len: u32,
	pub max_window_slots: u32,
	pub stakes: Vec<String>,
	pub root: String,
	pub leaders: Vec<u32>,
	pub windows: Vec<u32>,
}

/// The collection of golden scenarios.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GoldenVectors {
	pub scenarios: Vec<GoldenScenario>,
}

fn dummy_id(i: u8) -> SunLeadId {
	let mut b = [0u8; 32];
	b[0] = i;
	b[1] = i.wrapping_mul(7);
	SunLeadId::try_from(&b[..]).expect("32 bytes")
}

fn h256_repeat(b: u8) -> H256 {
	H256::from_slice(&[b; 32])
}

/// Build all golden scenarios from fixed parameters. The schedule is computed by
/// the reference implementation, so the stored output is the canonical answer.
pub fn generate_golden_vectors() -> GoldenVectors {
	let def = SunLeadScheduleParams::v1_defaults();
	let scenarios = vec![
		scenario("single", 1, 1, 1, 42, &[100], def),
		scenario("equal-4", 1, 1, 1, 7, &[10, 10, 10, 10], def),
		scenario("skewed", 1, 1, 1, 3, &[900, 50, 30, 20], def),
		scenario("zero-stake", 1, 1, 1, 4, &[0, 0, 0, 0], def),
		scenario(
			"many-16",
			1,
			1,
			1,
			5,
			&(0..16).map(|i| 1 + i as u128).collect::<Vec<_>>(),
			def,
		),
		scenario(
			"long-epoch",
			1,
			1,
			1,
			6,
			&[100, 50, 30, 20],
			SunLeadScheduleParams { epoch_slots: 600, window_len: 2, max_window_slots: 4 },
		),
	];
	GoldenVectors { scenarios }
}

fn scenario(
	name: &str,
	chain_id: u64,
	set_id: u64,
	epoch: u64,
	seed_byte: u8,
	stakes: &[Stake],
	params: SunLeadScheduleParams,
) -> GoldenScenario {
	let input = ScheduleInput {
		chain_id,
		authority_set_id: set_id,
		epoch_index: epoch,
		epoch_seed: h256_repeat(seed_byte),
		authorities: stakes.iter().enumerate().map(|(i, s)| (dummy_id(i as u8), *s)).collect(),
		params,
	};
	let s = build_schedule(&input).expect("golden scenario must build");
	GoldenScenario {
		name: name.to_string(),
		chain_id,
		authority_set_id: set_id,
		epoch_index: epoch,
		epoch_seed: format!("{:?}", input.epoch_seed),
		epoch_slots: params.epoch_slots,
		window_len: params.window_len,
		max_window_slots: params.max_window_slots,
		stakes: stakes.iter().map(|s| s.to_string()).collect(),
		root: format!("{:?}", s.root),
		leaders: s.leaders,
		windows: s.windows,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::Path;

	const PATH: &str = "golden/sunlead.golden.json";

	#[test]
	fn golden_bootstrap_or_verify() {
		let vectors = generate_golden_vectors();
		let json = serde_json::to_string_pretty(&vectors).expect("serialize golden");
		if Path::new(PATH).exists() {
			let existing = std::fs::read_to_string(PATH).expect("read golden");
			assert_eq!(existing.trim(), json.trim(), "golden vector mismatch");
		} else {
			std::fs::create_dir_all("golden").ok();
			std::fs::write(PATH, &json).expect("write golden");
		}
	}

	#[test]
	fn golden_scenarios_are_all_valid() {
		for sc in &generate_golden_vectors().scenarios {
			assert_eq!(sc.leaders.len() as u64, sc.epoch_slots);
			assert_eq!(sc.windows.len() as u64, sc.epoch_slots);
			assert!(!sc.root.is_empty());
		}
	}
}
