//! Lightning Network integration for Ark protocol through submarine swaps
//!
//! This crate provides lightning network integration for the Ark protocol through submarine swaps,
//! enabling seamless Lightning Network payments using VTXOs.

pub mod errors;
pub mod lightning_swaps;
pub mod traits;
pub mod types;
pub mod utils;

#[cfg(feature = "boltz")]
pub mod boltz;

#[cfg(feature = "bip353")]
pub mod bip353;

pub use errors::*;
pub use lightning_swaps::*;
pub use traits::*;
pub use types::*;
