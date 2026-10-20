//! ADL candidate selection (Phase 2 §8.5).
//!
//! Sorting every open position is an unbounded operation, so candidates are **proposed by a
//! permissionless keeper and verified by the runtime**. What makes that safe is that the *order*
//! is never the keeper's: verified candidates are ranked by `(roe desc, account asc)`, so any
//! two keepers proposing the same set produce byte-identical outcomes.
//!
//! This module is deliberately a pure function over a ranking closure, so the ordering rule can
//! be unit tested without storage, a runtime or an exchange core.

use codec::Encode;
use exchange_primitives::AdlRank;
use sp_std::vec::Vec;

/// Verifies keeper-proposed ADL candidates and returns them in deterministic execution order.
///
/// `bankrupt_sign` is the sign of the position being offset: `+1` when a long went bankrupt, so
/// the candidates must be short. An account is dropped unless it holds the opposite side, is
/// profitable, and clears `min_roe_ppm`.
pub fn select_adl_targets<AccountId: Encode + Clone>(
	proposed: impl IntoIterator<Item = AccountId>,
	rank: impl Fn(&AccountId) -> Option<AdlRank>,
	bankrupt_sign: i64,
	min_roe_ppm: i64,
	max: usize,
) -> Vec<AccountId> {
	// A bankrupt position whose loss was realised by the mark (rather than by funding) ends
	// up with `side_sign == 0`. That would make the opposite-side test below vacuous and let
	// *both* sides qualify, so fail closed: without a known direction there is no rule to
	// apply and nobody may be forced out.
	if bankrupt_sign != 1 && bankrupt_sign != -1 {
		return Vec::new()
	}

	let mut verified: Vec<(AdlRank, Vec<u8>, AccountId)> = proposed
		.into_iter()
		.filter_map(|who| {
			let r = rank(&who)?;
			// Must hold a position, on the opposite side, and be profitable.
			if r.size_lots.signum() == 0 || r.size_lots.signum() == bankrupt_sign {
				return None
			}
			if r.unrealized_pnl <= 0 || r.roe_ppm < min_roe_ppm {
				return None
			}
			Some((r, who.encode(), who))
		})
		.collect();

	// (roe desc, account asc) — the account's SCALE encoding is the tie-break because
	// `AccountId` is not guaranteed to be `Ord`.
	verified.sort_by(|a, b| b.0.roe_ppm.cmp(&a.0.roe_ppm).then_with(|| a.1.cmp(&b.1)));
	// A keeper must not be able to spend the whole budget on one account by listing it
	// repeatedly. Sorting first makes duplicates adjacent, so removing them is deterministic.
	verified.dedup_by(|a, b| a.1 == b.1);
	verified.truncate(max);
	verified.into_iter().map(|(_, _, who)| who).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rank(size_lots: i64, unrealized_pnl: i128, roe_ppm: i64) -> AdlRank {
		AdlRank {
			size_lots,
			unrealized_pnl,
			roe_ppm,
			mark_price_ticks: 60_000,
			notional_per_lot: 60_000,
		}
	}

	#[test]
	fn profitable_opposite_side_is_selected() {
		let ranks = [(1u64, rank(-10, 500, 100_000))];
		let out = select_adl_targets([1u64], |w| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r), 1, 0, 8);
		assert_eq!(out, vec![1]);
	}

	#[test]
	fn same_side_is_rejected() {
		// Bankrupt long (+1); a long candidate must never be ADL'd.
		let ranks = [(1u64, rank(10, 500, 100_000))];
		let out = select_adl_targets([1u64], |w| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r), 1, 0, 8);
		assert!(out.is_empty());
	}

	#[test]
	fn unprofitable_and_below_min_roe_are_rejected() {
		let losing = [(1u64, rank(-10, -500, 100_000))];
		let out = select_adl_targets([1u64], |w| losing.iter().find(|(k, _)| k == w).map(|(_, r)| *r), 1, 0, 8);
		assert!(out.is_empty(), "a losing account must never be ADL'd");

		let thin = [(1u64, rank(-10, 500, 10_000))];
		let out = select_adl_targets([1u64], |w| thin.iter().find(|(k, _)| k == w).map(|(_, r)| *r), 1, 50_000, 8);
		assert!(out.is_empty(), "below MinAdlRoePpm");
	}

	#[test]
	fn duplicate_proposals_are_counted_once() {
		let ranks = [(1u64, rank(-10, 500, 50_000)), (2u64, rank(-10, 400, 40_000))];
		let rank_of = |w: &u64| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r);

		// Repeating one account must not multiply its share of the budget.
		let out = select_adl_targets([1u64, 1, 1], rank_of, 1, 0, 8);
		assert_eq!(out, vec![1], "a repeated account is one target");

		let out = select_adl_targets([1u64, 2, 1, 2], rank_of, 1, 0, 8);
		assert_eq!(out, vec![1, 2], "duplicates do not crowd out other accounts");
	}

	#[test]
	fn order_is_roe_desc_then_account_asc() {
		let ranks = [
			(7u64, rank(-10, 500, 50_000)),
			(3u64, rank(-10, 500, 50_000)),
			(9u64, rank(-10, 900, 90_000)),
		];
		let rank_of = |w: &u64| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r);
		// Deliberately submitted in an order that is neither.
		let out = select_adl_targets([7u64, 3, 9], rank_of, 1, 0, 8);
		assert_eq!(out, vec![9, 3, 7], "highest roe first, ties broken by account id");
	}

	#[test]
	fn submission_order_never_changes_the_result() {
		let ranks = [
			(7u64, rank(-10, 500, 50_000)),
			(3u64, rank(-10, 500, 50_000)),
			(9u64, rank(-10, 900, 90_000)),
		];
		let rank_of = |w: &u64| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r);
		let a = select_adl_targets([3u64, 9, 7], rank_of, 1, 0, 8);
		let b = select_adl_targets([9u64, 7, 3], rank_of, 1, 0, 8);
		assert_eq!(a, b);
	}

	#[test]
	fn result_is_capped() {
		let ranks = [
			(1u64, rank(-10, 500, 10_000)),
			(2u64, rank(-10, 500, 20_000)),
			(3u64, rank(-10, 500, 30_000)),
		];
		let rank_of = |w: &u64| ranks.iter().find(|(k, _)| k == w).map(|(_, r)| *r);
		let out = select_adl_targets([1u64, 2, 3], rank_of, 1, 0, 2);
		assert_eq!(out, vec![3, 2], "at most `max`, best first");
	}
}
