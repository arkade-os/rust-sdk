//! Messages exchanged between the client and the Ark server.

use crate::tx_graph::TxGraphChunk;
use crate::ArkAddress;
use crate::Error;
use ::serde::Deserialize;
use ::serde::Serialize;
use bitcoin::secp256k1::PublicKey;
use bitcoin::taproot::Signature;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::Txid;
use musig::musig;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// A public nonce per shared internal (non-leaf) node in the VTXO tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoncePks(#[serde(with = "serde::nonce_map")] HashMap<Txid, musig::PublicNonce>);

impl NoncePks {
    pub fn new(nonce_pks: HashMap<Txid, musig::PublicNonce>) -> Self {
        Self(nonce_pks)
    }

    /// Get the [`MusigPubNonce`] for the transaction identified by `txid`.
    pub fn get(&self, txid: &Txid) -> Option<musig::PublicNonce> {
        self.0.get(txid).copied()
    }
}

/// A Musig partial signature per shared internal (non-leaf) node in the VTXO tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartialSigTree(
    #[serde(with = "serde::partial_sig_map")] pub HashMap<Txid, musig::PartialSignature>,
);

#[derive(Debug, Clone, Default)]
pub struct TxTree {
    pub nodes: BTreeMap<(usize, usize), TxTreeNode>,
}

impl TxTree {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }

    pub fn get_mut(&mut self, level: usize, index: usize) -> Result<&mut TxTreeNode, Error> {
        self.nodes
            .get_mut(&(level, index))
            .ok_or_else(|| Error::ad_hoc("TxTreeNode not found at ({level}, {index})"))
    }

    pub fn insert(&mut self, node: TxTreeNode, level: usize, index: usize) {
        self.nodes.insert((level, index), node);
    }

    pub fn txs(&self) -> impl Iterator<Item = &Transaction> {
        self.nodes.values().map(|node| &node.tx.unsigned_tx)
    }

    /// Get all nodes at a specific level.
    pub fn get_level(&self, level: usize) -> Vec<&TxTreeNode> {
        self.nodes
            .range((level, 0)..(level + 1, 0))
            .map(|(_, node)| node)
            .collect()
    }

    /// Iterate over levels in order.
    pub fn iter_levels(&self) -> impl Iterator<Item = (usize, Vec<&TxTreeNode>)> {
        let max_level = self
            .nodes
            .keys()
            .map(|(level, _)| *level)
            .max()
            .unwrap_or(0);

        (0..=max_level).map(move |level| {
            let nodes = self.get_level(level);
            (level, nodes)
        })
    }
}

#[derive(Debug, Clone)]
pub struct TxTreeNode {
    pub txid: Txid,
    pub tx: Psbt,
    pub parent_txid: Txid,
    pub level: i32,
    pub level_index: i32,
    pub leaf: bool,
}

// TODO: Implement pagination.
pub struct GetVtxosRequest {
    reference: GetVtxosRequestReference,
    filter: Option<GetVtxosRequestFilter>,
}

impl GetVtxosRequest {
    pub fn new_for_addresses(addresses: &[ArkAddress]) -> Self {
        let scripts = addresses
            .iter()
            .map(|a| a.to_p2tr_script_pubkey())
            .collect();

        Self {
            reference: GetVtxosRequestReference::Scripts(scripts),
            filter: None,
        }
    }

    pub fn new_for_outpoints(outpoints: &[OutPoint]) -> Self {
        Self {
            reference: GetVtxosRequestReference::OutPoints(outpoints.to_vec()),
            filter: None,
        }
    }

    pub fn spendable_only(self) -> Result<Self, Error> {
        if self.filter.is_some() {
            return Err(Error::ad_hoc("GetVtxosRequest filter already set"));
        }

        Ok(Self {
            filter: Some(GetVtxosRequestFilter::Spendable),
            ..self
        })
    }

    pub fn spent_only(self) -> Result<Self, Error> {
        if self.filter.is_some() {
            return Err(Error::ad_hoc("GetVtxosRequest filter already set"));
        }

        Ok(Self {
            filter: Some(GetVtxosRequestFilter::Spent),
            ..self
        })
    }

    pub fn recoverable_only(self) -> Result<Self, Error> {
        if self.filter.is_some() {
            return Err(Error::ad_hoc("GetVtxosRequest filter already set"));
        }

        Ok(Self {
            filter: Some(GetVtxosRequestFilter::Recoverable),
            ..self
        })
    }

    pub fn reference(&self) -> &GetVtxosRequestReference {
        &self.reference
    }

    pub fn filter(&self) -> Option<&GetVtxosRequestFilter> {
        self.filter.as_ref()
    }
}

pub enum GetVtxosRequestReference {
    Scripts(Vec<ScriptBuf>),
    OutPoints(Vec<OutPoint>),
}

pub enum GetVtxosRequestFilter {
    Spendable,
    Spent,
    Recoverable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VirtualTxOutPoint {
    pub outpoint: OutPoint,
    pub created_at: i64,
    pub expires_at: i64,
    pub amount: Amount,
    pub script: ScriptBuf,
    /// A pre-confirmed VTXO spends from another VTXO and is not a leaf of the original VTXO tree
    /// in a batch.
    pub is_preconfirmed: bool,
    pub is_swept: bool,
    pub is_unrolled: bool,
    pub is_spent: bool,
    /// If the VTXO is spent, this field references the _checkpoint transaction_ that actually
    /// spends it. The corresponding Ark transaction is in the `ark_txid` field.
    ///
    /// If the VTXO is renewed, this field references the corresponding _forfeit transaction_.
    pub spent_by: Option<Txid>,
    /// The list of commitment transactions that are ancestors to this VTXO.
    pub commitment_txids: Vec<Txid>,
    /// The commitment TXID onto which this VTXO was forfeited.
    pub settled_by: Option<Txid>,
    /// The Ark transaction that _spends_ this VTXO (if we omit the checkpoint transaction).
    pub ark_txid: Option<Txid>,
}

impl VirtualTxOutPoint {
    pub fn is_recoverable(&self) -> bool {
        self.is_swept && !self.is_spent
    }
}

#[derive(Clone, Debug)]
pub struct Info {
    pub pk: PublicKey,
    pub vtxo_tree_expiry: bitcoin::Sequence,
    pub unilateral_exit_delay: bitcoin::Sequence,
    pub boarding_exit_delay: bitcoin::Sequence,
    pub round_interval: i64,
    pub network: Network,
    pub dust: Amount,
    pub forfeit_address: bitcoin::Address,
    pub version: String,
    pub utxo_min_amount: Option<Amount>,
    pub utxo_max_amount: Option<Amount>,
    pub vtxo_min_amount: Option<Amount>,
    pub vtxo_max_amount: Option<Amount>,
}

#[derive(Clone, Debug)]
pub struct ListVtxo {
    spent: Vec<VirtualTxOutPoint>,
    spendable: Vec<VirtualTxOutPoint>,
}

impl ListVtxo {
    pub fn new(spent: Vec<VirtualTxOutPoint>, spendable: Vec<VirtualTxOutPoint>) -> Self {
        Self { spent, spendable }
    }

    pub fn all(&self) -> Vec<VirtualTxOutPoint> {
        [self.spent(), self.spendable()].concat()
    }

    pub fn spent(&self) -> &[VirtualTxOutPoint] {
        &self.spent
    }

    pub fn spent_without_recoverable(&self) -> Vec<VirtualTxOutPoint> {
        self.spent
            .iter()
            .filter(|v| !v.is_recoverable())
            .cloned()
            .collect()
    }

    pub fn spendable(&self) -> &[VirtualTxOutPoint] {
        &self.spendable
    }

    pub fn spendable_with_recoverable(&self) -> Vec<VirtualTxOutPoint> {
        let mut spendable = self.spendable.clone();

        let mut recoverable_vtxos = Vec::new();
        for spent_vtxo in self.spent.iter() {
            if spent_vtxo.is_recoverable() {
                recoverable_vtxos.push(spent_vtxo.clone());
            }
        }

        spendable.append(&mut recoverable_vtxos);

        spendable
    }
}

#[derive(Debug, Clone)]
pub struct BatchStartedEvent {
    pub id: String,
    pub intent_id_hashes: Vec<String>,
    // TODO: Perhaps needs to be `bitcoin::Sequence`.
    pub batch_expiry: i64,
}

#[derive(Debug, Clone)]
pub struct BatchFinalizationEvent {
    pub id: String,
    pub commitment_tx: Psbt,
}

#[derive(Debug, Clone)]
pub struct BatchFinalizedEvent {
    pub id: String,
    pub commitment_txid: Txid,
}

#[derive(Debug, Clone)]
pub struct BatchFailed {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct TreeSigningStartedEvent {
    pub id: String,
    pub cosigners_pubkeys: Vec<PublicKey>,
    pub unsigned_commitment_tx: Psbt,
}

#[derive(Debug, Clone)]
pub struct TreeNoncesAggregatedEvent {
    pub id: String,
    pub tree_nonces: NoncePks,
}

#[derive(Debug, Clone)]
pub struct TreeTxEvent {
    pub id: String,
    pub topic: Vec<String>,
    pub batch_tree_event_type: BatchTreeEventType,
    pub tx_graph_chunk: TxGraphChunk,
}

#[derive(Debug, Clone)]
pub struct TreeSignatureEvent {
    pub id: String,
    pub topic: Vec<String>,
    pub batch_tree_event_type: BatchTreeEventType,
    pub txid: Txid,
    pub signature: Signature,
}

#[derive(Debug, Clone)]
pub enum BatchTreeEventType {
    Vtxo,
    Connector,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    BatchStarted(BatchStartedEvent),
    BatchFinalization(BatchFinalizationEvent),
    BatchFinalized(BatchFinalizedEvent),
    BatchFailed(BatchFailed),
    TreeSigningStarted(TreeSigningStartedEvent),
    TreeNoncesAggregated(TreeNoncesAggregatedEvent),
    TreeTx(TreeTxEvent),
    TreeSignature(TreeSignatureEvent),
}

pub enum StreamTransaction {
    Commitment(CommitmentTransaction),
    Ark(ArkTransaction),
}

pub struct ArkTransaction {
    pub txid: Txid,
    pub spent_vtxos: Vec<VirtualTxOutPoint>,
    pub spendable_vtxos: Vec<VirtualTxOutPoint>,
}

pub struct CommitmentTransaction {
    pub txid: Txid,
    pub spent_vtxos: Vec<VirtualTxOutPoint>,
    pub spendable_vtxos: Vec<VirtualTxOutPoint>,
}

#[derive(Clone, Debug)]
pub struct SubscriptionResponse {
    pub txid: Txid,
    pub scripts: Vec<String>,
    pub new_vtxos: Vec<VirtualTxOutPoint>,
    pub spent_vtxos: Vec<VirtualTxOutPoint>,
    pub tx: Option<Psbt>,
    pub checkpoint_txs: HashMap<String, Txid>,
}

pub struct VtxoChains {
    pub inner: Vec<VtxoChain>,
}

pub struct VtxoChain {
    pub txid: Txid,
    pub tx_type: ChainedTxType,
    pub spends: Vec<Txid>,
    pub expires_at: i64,
}

#[derive(Debug)]
pub enum ChainedTxType {
    Commitment,
    Tree,
    Checkpoint,
    Ark,
    Unspecified,
}

pub struct SubmitOffchainTxResponse {
    pub signed_ark_tx: Psbt,
    pub signed_checkpoint_txs: Vec<Psbt>,
}

#[derive(Debug, Clone)]
pub struct FinalizeOffchainTxResponse {}

mod serde {
    use super::*;
    use ::serde::de;
    use ::serde::Deserialize;
    use ::serde::Deserializer;
    use ::serde::Serialize;
    use ::serde::Serializer;
    use bitcoin::hex::DisplayHex;
    use std::collections::HashMap as StdHashMap;

    pub mod nonce_map {
        use super::*;

        pub fn serialize<S>(
            map: &HashMap<Txid, musig::PublicNonce>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let map_object: StdHashMap<String, String> = map
                .iter()
                .map(|(txid, nonce)| {
                    let hex_nonce = nonce.serialize().to_vec().to_lower_hex_string();
                    (txid.to_string(), hex_nonce)
                })
                .collect();

            map_object.serialize(serializer)
        }

        pub fn deserialize<'de, D>(
            deserializer: D,
        ) -> Result<HashMap<Txid, musig::PublicNonce>, D::Error>
        where
            D: Deserializer<'de>,
        {
            use de::Error;

            let map_object: StdHashMap<String, String> = StdHashMap::deserialize(deserializer)?;

            let mut nonce_pks = HashMap::new();

            for (txid_str, hex_nonce) in map_object {
                let txid = txid_str.parse().map_err(D::Error::custom)?;
                let nonce_bytes =
                    bitcoin::hex::FromHex::from_hex(&hex_nonce).map_err(D::Error::custom)?;
                let nonce =
                    musig::PublicNonce::from_byte_array(&nonce_bytes).map_err(D::Error::custom)?;
                nonce_pks.insert(txid, nonce);
            }

            Ok(nonce_pks)
        }
    }

    pub mod partial_sig_map {
        use super::*;

        pub fn serialize<S>(
            map: &HashMap<Txid, musig::PartialSignature>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let map_object: StdHashMap<String, String> = map
                .iter()
                .map(|(txid, sig)| {
                    let hex_sig = sig.serialize().to_vec().to_lower_hex_string();
                    (txid.to_string(), hex_sig)
                })
                .collect();

            map_object.serialize(serializer)
        }

        pub fn deserialize<'de, D>(
            deserializer: D,
        ) -> Result<HashMap<Txid, musig::PartialSignature>, D::Error>
        where
            D: Deserializer<'de>,
        {
            use de::Error;

            let map_object: StdHashMap<String, String> = StdHashMap::deserialize(deserializer)?;

            let mut partial_sigs = HashMap::new();

            for (txid_str, hex_sig) in map_object {
                let txid = txid_str.parse().map_err(D::Error::custom)?;
                let sig_bytes =
                    bitcoin::hex::FromHex::from_hex(&hex_sig).map_err(D::Error::custom)?;
                let sig = musig::PartialSignature::from_byte_array(&sig_bytes)
                    .map_err(D::Error::custom)?;
                partial_sigs.insert(txid, sig);
            }

            Ok(partial_sigs)
        }
    }
}
