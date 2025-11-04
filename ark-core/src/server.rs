//! Messages exchanged between the client and the Ark server.

use crate::tx_graph::TxGraphChunk;
use crate::ArkAddress;
use crate::Error;
use crate::ErrorContext;
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::PublicKey;
use bitcoin::taproot::Signature;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use musig::musig;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::str::FromStr;

/// An aggregate public nonce per shared internal (non-leaf) node in the VTXO tree.
#[derive(Debug, Clone)]
pub struct NoncePks(HashMap<Txid, musig::PublicNonce>);

impl NoncePks {
    pub fn new(nonce_pks: HashMap<Txid, musig::PublicNonce>) -> Self {
        Self(nonce_pks)
    }

    /// Get the [`MusigPubNonce`] for the transaction identified by `txid`.
    pub fn get(&self, txid: &Txid) -> Option<musig::PublicNonce> {
        self.0.get(txid).copied()
    }

    pub fn encode(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .map(|(k, v)| (k.to_string(), v.serialize().to_lower_hex_string()))
            .collect()
    }

    pub fn decode(map: HashMap<String, String>) -> Result<Self, Error> {
        let map = map
            .into_iter()
            .map(|(k, v)| {
                let key = k
                    .parse()
                    .map_err(Error::ad_hoc)
                    .context("failed to parse TXID")?;

                let value = {
                    let nonce_bytes = bitcoin::hex::FromHex::from_hex(&v)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode public nonce from hex")?;
                    musig::PublicNonce::from_byte_array(&nonce_bytes)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode public nonce from bytes")?
                };

                Ok((key, value))
            })
            .collect::<Result<HashMap<Txid, musig::PublicNonce>, Error>>()?;

        Ok(Self(map))
    }
}

/// A public nonce per public key, where each public key corresponds to a party signing a
/// transaction in the VTXO tree.
#[derive(Debug, Clone)]
pub struct TreeTxNoncePks(pub HashMap<XOnlyPublicKey, musig::PublicNonce>);

impl TreeTxNoncePks {
    pub fn new(tree_nonce_pks: HashMap<XOnlyPublicKey, musig::PublicNonce>) -> Self {
        Self(tree_nonce_pks)
    }

    pub fn to_pks(&self) -> Vec<musig::PublicNonce> {
        self.0.values().copied().collect()
    }

    pub fn encode(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .map(|(k, v)| (k.to_string(), v.serialize().to_lower_hex_string()))
            .collect()
    }

    pub fn decode(map: HashMap<String, String>) -> Result<Self, Error> {
        let map = map
            .into_iter()
            .map(|(k, v)| {
                let key = k
                    .parse()
                    .map_err(Error::ad_hoc)
                    .context("failed to parse PK")?;

                let value = {
                    let nonce_bytes = bitcoin::hex::FromHex::from_hex(&v)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode public nonce from hex")?;
                    musig::PublicNonce::from_byte_array(&nonce_bytes)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode public nonce from bytes")?
                };

                Ok((key, value))
            })
            .collect::<Result<HashMap<XOnlyPublicKey, musig::PublicNonce>, Error>>()?;

        Ok(Self(map))
    }
}

/// A Musig partial signature per shared internal (non-leaf) node in the VTXO tree.
#[derive(Debug, Clone, Default)]
pub struct PartialSigTree(pub HashMap<Txid, musig::PartialSignature>);

impl PartialSigTree {
    pub fn encode(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .map(|(k, v)| (k.to_string(), v.serialize().to_lower_hex_string()))
            .collect()
    }

    pub fn decode(map: HashMap<String, String>) -> Result<Self, Error> {
        let map = map
            .into_iter()
            .map(|(k, v)| {
                let key = k
                    .parse()
                    .map_err(Error::ad_hoc)
                    .context("failed to parse TXID")?;

                let value = {
                    let sig_bytes = bitcoin::hex::FromHex::from_hex(&v)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode partial signature from hex")?;
                    musig::PartialSignature::from_byte_array(&sig_bytes)
                        .map_err(Error::ad_hoc)
                        .context("failed to decode partial signature from bytes")?
                };

                Ok((key, value))
            })
            .collect::<Result<HashMap<Txid, musig::PartialSignature>, Error>>()?;

        Ok(Self(map))
    }
}

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
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("valid duration")
            .as_secs() as i64;

        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        let current_timestamp = {
            let window = web_sys::window().expect("should have a window in this context");
            let performance = window
                .performance()
                .expect("performance should be available");
            performance.now() as i64
        };

        (self.is_swept && !self.is_spent) || current_timestamp > self.expires_at
    }
}

#[derive(Clone, Debug)]
pub struct Info {
    pub version: String,
    pub signer_pk: PublicKey,
    pub forfeit_pk: PublicKey,
    pub forfeit_address: bitcoin::Address,
    pub checkpoint_tapscript: ScriptBuf,
    pub network: bitcoin::Network,
    pub session_duration: u64,
    pub unilateral_exit_delay: bitcoin::Sequence,
    pub boarding_exit_delay: bitcoin::Sequence,
    pub utxo_min_amount: Option<Amount>,
    pub utxo_max_amount: Option<Amount>,
    pub vtxo_min_amount: Option<Amount>,
    pub vtxo_max_amount: Option<Amount>,
    pub dust: Amount,
    pub fees: Option<FeeInfo>,
    pub scheduled_session: Option<ScheduledSession>,
    pub deprecated_signers: Vec<DeprecatedSigner>,
    pub service_status: HashMap<String, String>,
    pub digest: String,
}

// FIXME: Use proper types.
#[derive(Clone, Debug)]
pub struct FeeInfo {
    pub intent_fee: Option<IntentFeeInfo>,
    pub tx_fee_rate: String,
}
#[derive(Clone, Debug)]
pub struct IntentFeeInfo {
    pub offchain_input: String,
    pub offchain_output: String,
    pub onchain_input: String,
    pub onchain_output: String,
}

#[derive(Clone, Debug)]
pub struct ScheduledSession {
    pub next_start_time: i64,
    pub next_end_time: i64,
    pub period: i64,
    pub duration: i64,
    pub fees: Option<FeeInfo>,
}

#[derive(Clone, Debug)]
pub struct DeprecatedSigner {
    pub pk: PublicKey,
    pub cutoff_date: i64,
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

    /// Collect all unspent VTXOs, including recoverable ones.
    ///
    /// Useful when building a VTXO set that can be settled.
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

    /// Collect all unspent VTXOs, excluding recoverable ones.
    ///
    /// Useful when building a VTXO set that can be sent offchain, since recoverable VTXOs can only
    /// be settled.
    pub fn spendable_without_recoverable(&self) -> Vec<VirtualTxOutPoint> {
        self.spendable
            .clone()
            .into_iter()
            .filter(|v| !v.is_recoverable())
            .collect::<Vec<_>>()
    }
}

#[derive(Debug, Clone)]
pub struct BatchStartedEvent {
    pub id: String,
    pub intent_id_hashes: Vec<String>,
    pub batch_expiry: bitcoin::Sequence,
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
pub struct TreeNoncesEvent {
    pub id: String,
    pub topic: Vec<String>,
    pub txid: Txid,
    pub nonces: TreeTxNoncePks,
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
    TreeNonces(TreeNoncesEvent),
    Heartbeat,
}

impl StreamEvent {
    pub fn name(&self) -> String {
        let s = match self {
            StreamEvent::BatchStarted(_) => "BatchStarted",
            StreamEvent::BatchFinalization(_) => "BatchFinalization",
            StreamEvent::BatchFinalized(_) => "BatchFinalized",
            StreamEvent::BatchFailed(_) => "BatchFailed",
            StreamEvent::TreeSigningStarted(_) => "TreeSigningStarted",
            StreamEvent::TreeNoncesAggregated(_) => "TreeNoncesAggregated",
            StreamEvent::TreeTx(_) => "TreeTx",
            StreamEvent::TreeSignature(_) => "TreeSignature",
            StreamEvent::TreeNonces(_) => "TreeNoncesEvent",
            StreamEvent::Heartbeat => "Heartbeat",
        };

        s.to_string()
    }
}

pub enum StreamTransactionData {
    Commitment(CommitmentTransaction),
    Ark(ArkTransaction),
    Heartbeat,
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
pub enum SubscriptionResponse {
    Event(Box<SubscriptionEvent>),
    Heartbeat,
}

#[derive(Clone, Debug)]
pub struct SubscriptionEvent {
    pub txid: Txid,
    pub scripts: Vec<ScriptBuf>,
    pub new_vtxos: Vec<VirtualTxOutPoint>,
    pub spent_vtxos: Vec<VirtualTxOutPoint>,
    pub tx: Option<Psbt>,
    pub checkpoint_txs: HashMap<OutPoint, Txid>,
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

#[derive(Debug)]
pub struct VirtualTxsResponse {
    pub txs: Vec<Psbt>,
    pub page: Option<IndexerPage>,
}

#[derive(Debug)]
pub struct IndexerPage {
    pub current: i32,
    pub next: i32,
    pub total: i32,
}

#[derive(Clone, Debug)]
pub enum Network {
    Bitcoin,
    Testnet,
    Testnet4,
    Signet,
    Regtest,
    Mutinynet,
}

impl From<Network> for bitcoin::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Testnet => bitcoin::Network::Testnet,
            Network::Testnet4 => bitcoin::Network::Testnet4,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
            Network::Mutinynet => bitcoin::Network::Signet,
        }
    }
}

impl FromStr for Network {
    type Err = String;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bitcoin" => Ok(Network::Bitcoin),
            "testnet" => Ok(Network::Testnet),
            "testnet4" => Ok(Network::Testnet4),
            "signet" => Ok(Network::Signet),
            "regtest" => Ok(Network::Regtest),
            "mutinynet" => Ok(Network::Mutinynet),
            _ => Err(format!("Unsupported network {}", s.to_owned())),
        }
    }
}

pub fn parse_sequence_number(value: i64) -> Result<bitcoin::Sequence, Error> {
    /// The threshold that determines whether an expiry or exit delay should be parsed as a
    /// number of blocks or a number of seconds.
    ///
    /// - A value below 512 is considered a number of blocks.
    /// - A value of 512 or more is considered a number of seconds.
    const ARBITRARY_SEQUENCE_THRESHOLD: i64 = 512;

    let sequence = if value.is_negative() {
        return Err(Error::ad_hoc(format!("invalid sequence number: {value}")));
    } else if value < ARBITRARY_SEQUENCE_THRESHOLD {
        bitcoin::Sequence::from_height(value as u16)
    } else {
        let secs = u32::try_from(value)
            .map_err(|_| Error::ad_hoc(format!("sequence seconds overflow: {value}")))?;

        bitcoin::Sequence::from_seconds_ceil(secs).map_err(Error::ad_hoc)?
    };

    Ok(sequence)
}
