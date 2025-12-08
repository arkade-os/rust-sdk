use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::TxOut;

pub mod batch;
pub mod boarding_output;
pub mod coin_select;
pub mod conversions;
pub mod history;
pub mod intent;
pub mod script;
pub mod send;
pub mod server;
pub mod unilateral_exit;
pub mod vhtlc;
pub mod vtxo;

mod ark_address;
mod error;
mod tree_tx_output_script;
mod tx_graph;

pub use ark_address::ArkAddress;
pub use boarding_output::BoardingOutput;
pub use error::Error;
pub use error::ErrorContext;
pub use script::extract_sequence_from_csv_sig_script;
pub use tx_graph::TxGraph;
pub use tx_graph::TxGraphChunk;
pub use unilateral_exit::SelectedUtxo;
pub use unilateral_exit::UtxoCoinSelection;
pub use unilateral_exit::build_anchor_tx;
pub use unilateral_exit::build_unilateral_exit_tree_txids;
pub use vtxo::Vtxo;

pub const UNSPENDABLE_KEY: &str =
    "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0";

pub const VTXO_INPUT_INDEX: usize = 0;

/// The byte value corresponds to the string "taptree".
pub const VTXO_TAPROOT_KEY: [u8; 7] = [116, 97, 112, 116, 114, 101, 101];

/// The byte value corresponds to the string "condition".
pub const VTXO_CONDITION_KEY: [u8; 9] = [99, 111, 110, 100, 105, 116, 105, 111, 110];

/// The byte value corresponds to the string "expiry".
pub const VTXO_TREE_EXPIRY_PSBT_KEY: [u8; 6] = [101, 120, 112, 105, 114, 121];

/// The cosigner PKs that sign a VTXO TX input are included in the `unknown` key-value map field of
/// that input in the VTXO PSBT. Since the `unknown` field can be used for any purpose, we know that
/// a value is a cosigner PK if the corresponding key starts with this prefix.
///
/// The byte value corresponds to the string "cosigner".
pub const VTXO_COSIGNER_PSBT_KEY: [u8; 8] = [99, 111, 115, 105, 103, 110, 101, 114];

pub const DEFAULT_DERIVATION_PATH: &str = "m/83696968'/11811'/0";

const ANCHOR_SCRIPT_PUBKEY: [u8; 4] = [0x51, 0x02, 0x4e, 0x73];

/// Information a UTXO that may be extracted from an on-chain explorer.
#[derive(Clone, Copy, Debug)]
pub struct ExplorerUtxo {
    pub outpoint: OutPoint,
    pub amount: Amount,
    pub confirmation_blocktime: Option<u64>,
    pub is_spent: bool,
}

pub fn anchor_output() -> TxOut {
    let script_pubkey = ScriptBuf::from_bytes(ANCHOR_SCRIPT_PUBKEY.to_vec());

    TxOut {
        value: Amount::ZERO,
        script_pubkey,
    }
}
