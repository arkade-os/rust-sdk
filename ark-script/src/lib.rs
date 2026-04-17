//! Arkade script extension support.
//!
//! Arkade is a scripting extension layered on top of standard Bitcoin
//! script. This crate provides the pieces needed to build and sign
//! arkade-enhanced VTXOs:
//!
//! - Arkade extension opcode constants and ASM conversion
//! - BIP-340 tagged hashes used to derive introspector-bound public keys
//! - Helpers for assembling arkade tapscript leaves
//! - PSBT fields for carrying arkade scripts alongside a spend
//!
//! The crate depends on `ark-core` for types but is opt-in — consumers
//! who only need standard Ark functionality don't pay for it.

pub mod opcodes;
pub mod script;

pub use opcodes::op;
pub use opcodes::opcode_from_name;
pub use opcodes::opcode_name;
pub use opcodes::ARKADE_OPCODES;
pub use script::bytes_to_asm;
pub use script::from_asm;
pub use script::to_asm;
pub use script::AsmError;
