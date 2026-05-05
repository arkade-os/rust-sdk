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
pub mod tweak;

pub use opcodes::op;
pub use opcodes::opcode_from_name;
pub use opcodes::opcode_name;
pub use opcodes::ARKADE_OPCODES;
pub use script::bytes_to_asm;
pub use script::from_asm;
pub use script::to_asm;
pub use script::AsmError;
pub use tweak::arkade_script_hash;
pub use tweak::arkade_witness_hash;
pub use tweak::compute_arkade_script_public_key;
pub use tweak::ArkadeScriptHash;
pub use tweak::ArkadeWitnessHash;
pub use tweak::TweakError;

pub mod tapscript;
pub mod vtxo_script;

pub use tapscript::ArkadeTapscript;
pub use tapscript::TapscriptError;
pub use vtxo_script::ArkadeLeaf;
pub use vtxo_script::ArkadeVtxoInput;
pub use vtxo_script::ArkadeVtxoScript;
pub use vtxo_script::VtxoScriptError;
