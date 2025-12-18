use crate::script::csv_sig_script;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;

/// The script of an _internal_ node of the batch tree. By internal node we mean a non-leaf node.
///
/// This script allows the Ark server to sweep the entire output after the `vtxo_tree_expiry`
/// seconds have passed from the time the output was included in a block.
pub struct TreeTxOutputScript {
    script: ScriptBuf,
}

impl TreeTxOutputScript {
    pub fn new(vtxo_tree_expiry: bitcoin::Sequence, server: XOnlyPublicKey) -> Self {
        let script = csv_sig_script(vtxo_tree_expiry, server);

        Self { script }
    }

    pub fn sweep_spend_leaf(
        &self,
        secp: &Secp256k1<All>,
        aggregate_pk: XOnlyPublicKey,
    ) -> TaprootSpendInfo {
        TaprootBuilder::new()
            .add_leaf_with_ver(0, self.script.clone(), LeafVersion::TapScript)
            .expect("valid sweep leaf")
            .finalize(secp, aggregate_pk)
            .expect("can be finalized")
    }
}
