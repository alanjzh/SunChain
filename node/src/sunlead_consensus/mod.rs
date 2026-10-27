//! SunLead node-side consensus integration (parallel module, gated, BABE default).
//!
//! This module provides the machinery to run SunLead authoring + verification
//! alongside the existing BABE/GRANDPA stack. It is *not* the default authoring
//! engine; selection happens in `service.rs` based on the runtime activation
//! flag (`SunLeadApi::is_active`).

pub mod block_import;
pub mod import_queue;
pub mod worker;

pub use block_import::SunLeadBlockImport;
pub use import_queue::{sunlead_import_queue, SunLeadVerifier};
pub use worker::{start_sunlead, SunLeadAuxData, SunLeadClaim, SunLeadParams, SunLeadSlotWorker};
