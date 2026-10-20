//! Shared types and traits for the Sunchain exchange (Phase 2).
//!
//! Deliberately dependency-free apart from `sp-runtime`, so every pallet can depend on it
//! without creating cycles.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

mod price;
mod traits;
mod types;

pub use price::*;
pub use traits::*;
pub use types::*;
