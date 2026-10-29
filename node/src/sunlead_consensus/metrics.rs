//! Prometheus metrics for SunLead pre-import block verification.
//!
//! These are node-side (no runtime change) and expose the observable half of what
//! the verifier enforces: how many SunLead blocks passed verification, how many
//! were rejected and *why*, and how many equivocation attempts were seen. They are
//! the P1-7.3 diagnostics for the acceptance path — the part an operator most
//! needs to watch (a sudden spike in `sunlead_blocks_rejected_total{reason="..."}`
//! or `sunlead_equivocations_total` is the first signal of a misconfigured or
//! malicious peer).

use prometheus_endpoint::{
	prometheus::{Counter, CounterVec, Opts},
	Registry,
};

/// Node-side SunLead verification metrics.
///
/// Registered against the node's prometheus registry when one is configured;
/// otherwise the counters still increment in memory (they just are not exported).
pub struct SunLeadMetrics {
	/// SunLead blocks that passed pre-import verification.
	pub blocks_verified: Counter,
	/// SunLead blocks rejected by pre-import verification, labelled by reason.
	pub blocks_rejected: CounterVec,
	/// Detected SunLead equivocations (two different seals for the same slot).
	pub equivocations: Counter,
}

impl SunLeadMetrics {
	pub fn new(registry: Option<&Registry>) -> Self {
		let blocks_verified = Counter::new(
			"sunlead_blocks_verified_total",
			"SunLead blocks that passed pre-import verification.",
		)
		.expect("metric name is valid");
		let blocks_rejected = CounterVec::new(
			Opts::new(
				"sunlead_blocks_rejected_total",
				"SunLead blocks rejected by pre-import verification, by reason.",
			),
			&["reason"],
		)
		.expect("metric name and labels are valid");
		let equivocations = Counter::new(
			"sunlead_equivocations_total",
			"Detected SunLead equivocations (two different seals for the same slot).",
		)
		.expect("metric name is valid");

		if let Some(registry) = registry {
			// Registration fails only if a metric with the same name already
			// exists — a programming error, not a runtime condition.
			let _ = registry.register(Box::new(blocks_verified.clone()));
			let _ = registry.register(Box::new(blocks_rejected.clone()));
			let _ = registry.register(Box::new(equivocations.clone()));
		}

		Self { blocks_verified, blocks_rejected, equivocations }
	}

	/// Record a successfully verified SunLead block.
	pub fn record_verified(&self) {
		self.blocks_verified.inc();
	}

	/// Record a rejected SunLead block, classified by `reason`.
	pub fn record_rejected(&self, reason: &'static str) {
		self.blocks_rejected.with_label_values(&[reason]).inc();
	}

	/// Record a detected equivocation.
	pub fn record_equivocation(&self) {
		self.equivocations.inc();
	}
}

impl Clone for SunLeadMetrics {
	fn clone(&self) -> Self {
		Self {
			blocks_verified: self.blocks_verified.clone(),
			blocks_rejected: self.blocks_rejected.clone(),
			equivocations: self.equivocations.clone(),
		}
	}
}

/// Map a verification error string to a short, stable Prometheus label.
///
/// The verifier returns human-readable `String` errors; this collapses them to a
/// fixed set of `reason` values so the `blocks_rejected_total` counter stays
/// low-cardinality (a requirement for any labelled counter).
pub fn reason_from_error(err: &str) -> &'static str {
	if err.contains("schedule snapshot not available") {
		"no_snapshot"
	} else if err.contains("failed to fetch snapshot") {
		"snapshot_fetch"
	} else if err.contains("pre-digest verification failed") {
		"bad_pre_digest"
	} else if err.contains("undecodable") && err.contains("pre-runtime") {
		"bad_pre_digest_decode"
	} else if err.contains("seal verification failed") {
		"bad_seal"
	} else if err.contains("undecodable") && err.contains("seal") {
		"bad_seal_decode"
	} else if err.contains("parent hash mismatch") {
		"parent_hash"
	} else if err.contains("slot monotonicity violated") {
		"slot_monotonic"
	} else if err.contains("failed to fetch parent header") {
		"parent_fetch"
	} else if err.contains("parent header not found") {
		"no_parent"
	} else if err.contains("no sunlead pre-runtime digest found") {
		"missing_pre_digest"
	} else if err.contains("multiple sunlead pre-runtime digests") {
		"dup_pre_digest"
	} else if err.contains("no sunlead seal found") {
		"missing_seal"
	} else if err.contains("multiple sunlead seals") {
		"dup_seal"
	} else if err.contains("foreign") {
		"foreign_engine"
	} else {
		"other"
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use prometheus_endpoint::Registry;

	#[test]
	fn reason_from_error_collapses_to_stable_labels() {
		assert_eq!(reason_from_error("sunlead: schedule snapshot not available"), "no_snapshot");
		assert_eq!(reason_from_error("sunlead: failed to fetch snapshot: ..."), "snapshot_fetch");
		assert_eq!(reason_from_error("sunlead: pre-digest verification failed: ..."), "bad_pre_digest");
		assert_eq!(
			reason_from_error("sunlead pre-runtime digest present but undecodable"),
			"bad_pre_digest_decode"
		);
		assert_eq!(reason_from_error("sunlead: seal verification failed: ..."), "bad_seal");
		assert_eq!(reason_from_error("sunlead: parent hash mismatch: ..."), "parent_hash");
		assert_eq!(reason_from_error("sunlead: slot monotonicity violated: ..."), "slot_monotonic");
		assert_eq!(reason_from_error("no sunlead pre-runtime digest found"), "missing_pre_digest");
		assert_eq!(reason_from_error("unexpected foreign pre-runtime digest for engine"), "foreign_engine");
		assert_eq!(reason_from_error("some totally unknown error"), "other");
	}

	#[test]
	fn metrics_build_and_count_with_or_without_registry() {
		// Without a registry the metrics still construct and increment in memory.
		let m = SunLeadMetrics::new(None);
		m.record_verified();
		m.record_rejected("bad_seal");
		m.record_equivocation();
		// With a registry they register without panic.
		let reg = Registry::default();
		let m2 = SunLeadMetrics::new(Some(&reg));
		m2.record_verified();
	}
}
