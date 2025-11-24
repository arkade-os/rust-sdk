use crate::anchor_output;
use crate::conversions::from_musig_xonly;
use crate::conversions::to_musig_pk;
use crate::server::NoncePks;
use crate::server::PartialSigTree;
use crate::server::TreeTxNoncePks;
use crate::tree_tx_output_script::TreeTxOutputScript;
use crate::BoardingOutput;
use crate::Error;
use crate::ErrorContext;
use crate::TxGraph;
use crate::Vtxo;
use crate::VTXO_COSIGNER_PSBT_KEY;
use crate::VTXO_INPUT_INDEX;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::transaction;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use musig::musig;
use rand::CryptoRng;
use rand::Rng;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// A UTXO that is primed to become a VTXO. Alternatively, the owner of this UTXO may decide to
/// spend it into a vanilla UTXO.
///
/// Only UTXOs with a particular script (involving an Ark server) can become VTXOs.
#[derive(Debug, Clone)]
pub struct OnChainInput {
    /// The information needed to spend the UTXO.
    boarding_output: BoardingOutput,
    /// The amount of coins locked in the UTXO.
    amount: Amount,
    /// The location of this UTXO in the blockchain.
    outpoint: OutPoint,
}

impl OnChainInput {
    pub fn new(boarding_output: BoardingOutput, amount: Amount, outpoint: OutPoint) -> Self {
        Self {
            boarding_output,
            amount,
            outpoint,
        }
    }

    pub fn boarding_output(&self) -> &BoardingOutput {
        &self.boarding_output
    }

    pub fn amount(&self) -> Amount {
        self.amount
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }
}

/// Either a confirmed VTXO that needs to be renewed, or an pre-confirmed VTXO that needs
/// confirmation.
///
/// Alternatively, the owner of this VTXO may decide to spend it into a vanilla UTXO.
#[derive(Debug, Clone)]
pub struct VtxoInput {
    /// The information needed to spend the VTXO, besides the amount.
    vtxo: Vtxo,
    /// The amount of coins locked in the VTXO.
    amount: Amount,
    /// Where the VTXO would end up on the blockchain if it were to become a UTXO.
    outpoint: OutPoint,
}

impl VtxoInput {
    pub fn new(vtxo: Vtxo, amount: Amount, outpoint: OutPoint) -> Self {
        Self {
            vtxo,
            amount,
            outpoint,
        }
    }

    pub fn vtxo(&self) -> &Vtxo {
        &self.vtxo
    }

    pub fn amount(&self) -> Amount {
        self.amount
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }
}

/// A nonce key pair per tree transaction output that we are a part of in the batch.
///
/// The [`musig::SecretNonce`] element of the tuple is an [`Option`] because it cannot be cloned or
/// copied. When we are ready to sign a tree transaction, we call the method `take_sk` to move out
/// of the [`Option`].
#[allow(clippy::type_complexity)]
pub struct NonceKps(HashMap<Txid, (Option<musig::SecretNonce>, musig::PublicNonce)>);

impl NonceKps {
    /// Take ownership of the [`musig::SecretNonce`] for the transaction identified by `txid`.
    ///
    /// The caller must take ownership because the [`musig::SecretNonce`] ensures that it can only
    /// be used once, to avoid nonce reuse.
    pub fn take_sk(&mut self, txid: &Txid) -> Option<musig::SecretNonce> {
        self.0.get_mut(txid).and_then(|(sec, _)| sec.take())
    }

    /// Convert into [`NoncePks`].
    pub fn to_nonce_pks(&self) -> NoncePks {
        let nonce_pks = self
            .0
            .iter()
            .map(|(txid, (_, pub_nonce))| (*txid, *pub_nonce))
            .collect::<HashMap<_, _>>();

        NoncePks::new(nonce_pks)
    }
}

/// Generate a nonce key pair for each tree transaction output that we are a part of in the batch.
pub fn generate_nonce_tree<R>(
    rng: &mut R,
    batch_tree_tx_graph: &TxGraph,
    own_cosigner_pk: PublicKey,
    commitment_tx: &Psbt,
) -> Result<NonceKps, Error>
where
    R: Rng + CryptoRng,
{
    let secp_musig = ::musig::Secp256k1::new();

    let batch_tree_tx_map = batch_tree_tx_graph.as_map();

    let nonce_tree = batch_tree_tx_map
        .iter()
        .map(|(txid, tx)| {
            let cosigner_pks = extract_cosigner_pks_from_vtxo_psbt(tx)?;

            if !cosigner_pks.contains(&own_cosigner_pk) {
                return Err(Error::crypto(format!(
                    "cosigner PKs does not contain {own_cosigner_pk} for tree TX {txid}"
                )));
            }

            // TODO: We would like to use our own RNG here, but this library is using a
            // different version of `rand`. I think it's not worth the hassle at this stage,
            // particularly because this duplicated dependency will go away anyway.
            let session_id = musig::SessionSecretRand::new();
            let extra_rand = rng.gen();

            let msg = tree_tx_sighash(tx, &batch_tree_tx_map, commitment_tx)?;

            let key_agg_cache = {
                let cosigner_pks = cosigner_pks
                    .iter()
                    .map(|pk| to_musig_pk(*pk))
                    .collect::<Vec<_>>();
                musig::KeyAggCache::new(&secp_musig, &cosigner_pks.iter().collect::<Vec<_>>())
            };

            let (nonce, pub_nonce) = key_agg_cache.nonce_gen(
                &secp_musig,
                session_id,
                to_musig_pk(own_cosigner_pk),
                msg,
                extra_rand,
            );

            Ok((*txid, (Some(nonce), pub_nonce)))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;

    Ok(NonceKps(nonce_tree))
}

fn tree_tx_sighash(
    // The tree PSBT to be signed.
    psbt: &Psbt,
    // The entire tree TX set for this batch, to look for the previous output.
    tx_map: &HashMap<Txid, &Psbt>,
    // The commitment transaction, in case it contains the previous output.
    commitment_tx: &Psbt,
) -> Result<::musig::Message, Error> {
    let tx = &psbt.unsigned_tx;

    // We expect a single input to a VTXO.
    let previous_output = tx.input[VTXO_INPUT_INDEX].previous_output;

    let parent_tx = tx_map
        .get(&previous_output.txid)
        .or_else(|| {
            (previous_output.txid == commitment_tx.unsigned_tx.compute_txid())
                .then_some(&commitment_tx)
        })
        .ok_or_else(|| {
            Error::crypto(format!(
                "parent transaction {} not found for tree TX {}",
                previous_output.txid,
                tx.compute_txid()
            ))
        })?;
    let previous_output = parent_tx
        .unsigned_tx
        .output
        .get(previous_output.vout as usize)
        .ok_or_else(|| {
            Error::crypto(format!(
                "previous output {} not found for tree TX {}",
                previous_output,
                tx.compute_txid()
            ))
        })?;

    let prevouts = [previous_output];
    let prevouts = Prevouts::All(&prevouts);

    // Here we are generating a key spend sighash, because batch tree outputs are signed by parties
    // with VTXOs in this new batch. We use a musig key spend to efficiently coordinate with all the
    // parties.
    let tap_sighash = SighashCache::new(tx)
        .taproot_key_spend_signature_hash(VTXO_INPUT_INDEX, &prevouts, TapSighashType::Default)
        .map_err(Error::crypto)?;
    let msg = ::musig::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

    Ok(msg)
}

/// Compute the aggregated nonce public key for a transaction in the VTXO tree.
///
/// The [`TreeTxNoncePks`] holds the public nonces of all the cosigners of this transaction.
pub fn aggregate_nonces(tree_tx_nonce_pks: TreeTxNoncePks) -> musig::AggregatedNonce {
    let secp_musig = ::musig::Secp256k1::new();

    let pks = tree_tx_nonce_pks.to_pks();
    let ref_pks = pks.iter().collect::<Vec<_>>();
    musig::AggregatedNonce::new(&secp_musig, &ref_pks)
}

/// Use `own_cosigner_kp` to sign each batch tree transaction output that we are a part, using
/// `our_nonce_kps` to provide our share of each aggregate nonce.
#[allow(clippy::too_many_arguments)]
pub fn sign_batch_tree_tx(
    tree_txid: Txid,
    vtxo_tree_expiry: bitcoin::Sequence,
    server_pk: XOnlyPublicKey,
    own_cosigner_kp: &Keypair,
    agg_nonce_pk: musig::AggregatedNonce,
    batch_tree_tx_graph: &TxGraph,
    commitment_psbt: &Psbt,
    // This holds all the nonce KPs we generated earlier. We need to mutate it to be able to _move_
    // the secret nonce out of it before signing.
    our_nonce_kps: &mut NonceKps,
) -> Result<PartialSigTree, Error> {
    let own_cosigner_pk = own_cosigner_kp.public_key();

    let internal_node_script = TreeTxOutputScript::new(vtxo_tree_expiry, server_pk);

    let secp = Secp256k1::new();
    let secp_musig = ::musig::Secp256k1::new();

    let own_cosigner_kp =
        ::musig::Keypair::from_seckey_slice(&secp_musig, &own_cosigner_kp.secret_bytes())
            .expect("valid keypair");

    let batch_tree_tx_map = batch_tree_tx_graph.as_map();

    let psbt = batch_tree_tx_map
        .get(&tree_txid)
        .ok_or_else(|| Error::ad_hoc(format!("TXID {tree_txid} not found in batch tree map")))?;

    let mut cosigner_pks = extract_cosigner_pks_from_vtxo_psbt(psbt)?;
    cosigner_pks.sort_by_key(|k| k.serialize());

    if !cosigner_pks.contains(&own_cosigner_pk) {
        return Err(Error::ad_hoc(
            "own cosigner PK not found among tree transaction cosigner PKs",
        ));
    }

    tracing::debug!(%tree_txid, "Generating partial signature");

    let mut key_agg_cache = {
        let cosigner_pks = cosigner_pks
            .iter()
            .map(|pk| to_musig_pk(*pk))
            .collect::<Vec<_>>();
        musig::KeyAggCache::new(&secp_musig, &cosigner_pks.iter().collect::<Vec<_>>())
    };

    let sweep_tap_tree =
        internal_node_script.sweep_spend_leaf(&secp, from_musig_xonly(key_agg_cache.agg_pk()));

    let tweak = ::musig::Scalar::from(
        ::musig::SecretKey::from_slice(sweep_tap_tree.tap_tweak().as_byte_array())
            .expect("valid conversion"),
    );

    key_agg_cache
        .pubkey_xonly_tweak_add(&secp_musig, &tweak)
        .map_err(Error::crypto)?;

    let msg = tree_tx_sighash(psbt, &batch_tree_tx_map, commitment_psbt)?;

    let nonce_sk = our_nonce_kps
        .take_sk(&tree_txid)
        .ok_or_else(|| Error::crypto(format!("missing nonce for tree TX {tree_txid}")))?;

    let sig = musig::Session::new(&secp_musig, &key_agg_cache, agg_nonce_pk, msg).partial_sign(
        &secp_musig,
        nonce_sk,
        &own_cosigner_kp,
        &key_agg_cache,
    );

    let partial_sig_tree = HashMap::from_iter([(tree_txid, sig)]);

    Ok(PartialSigTree(partial_sig_tree))
}

/// Build and sign a forfeit transaction per [`VtxoInput`] to be used in an upcoming commitment
/// transaction.
pub fn create_and_sign_forfeit_txs<F>(
    vtxo_inputs: &[VtxoInput],
    connectors_leaves: &[&Psbt],
    server_forfeit_address: &Address,
    // As defined by the server.
    dust: Amount,
    sign: F,
) -> Result<Vec<Psbt>, Error>
where
    F: Fn(&secp256k1::Message, &Vtxo) -> (schnorr::Signature, XOnlyPublicKey),
{
    const FORFEIT_TX_CONNECTOR_INDEX: usize = 0;
    const FORFEIT_TX_VTXO_INDEX: usize = 1;

    let secp = Secp256k1::new();

    let connector_amount = dust;

    let connector_index = derive_vtxo_connector_map(vtxo_inputs, connectors_leaves, dust)?;

    let mut signed_forfeit_psbts = Vec::new();
    for VtxoInput {
        vtxo,
        amount: vtxo_amount,
        outpoint: virtual_tx_outpoint,
    } in vtxo_inputs.iter()
    {
        if *vtxo_amount <= dust {
            // Sub-dust VTXOs don't need to be forfeited.
            continue;
        }

        let connector_outpoint = connector_index.get(virtual_tx_outpoint).ok_or_else(|| {
            Error::ad_hoc(format!(
                "connector outpoint missing for virtual TX outpoint {virtual_tx_outpoint}"
            ))
        })?;

        let connector_psbt = connectors_leaves
            .iter()
            .find(|l| l.unsigned_tx.compute_txid() == connector_outpoint.txid)
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "connector PSBT missing for virtual TX outpoint {virtual_tx_outpoint}"
                ))
            })?;

        let connector_output = connector_psbt
            .unsigned_tx
            .output
            .get(connector_outpoint.vout as usize)
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "connector output missing for virtual TX outpoint {virtual_tx_outpoint}"
                ))
            })?;

        let forfeit_output = TxOut {
            value: *vtxo_amount + connector_amount,
            script_pubkey: server_forfeit_address.script_pubkey(),
        };

        let mut forfeit_psbt = Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: *connector_outpoint,
                    ..Default::default()
                },
                TxIn {
                    previous_output: *virtual_tx_outpoint,
                    ..Default::default()
                },
            ],
            output: vec![forfeit_output.clone(), anchor_output()],
        })
        .map_err(Error::transaction)?;

        forfeit_psbt.inputs[FORFEIT_TX_CONNECTOR_INDEX].witness_utxo =
            Some(connector_output.clone());

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].witness_utxo = Some(TxOut {
            value: *vtxo_amount,
            script_pubkey: vtxo.script_pubkey(),
        });

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].sighash_type =
            Some(TapSighashType::Default.into());

        let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info()?;

        let leaf_version = forfeit_control_block.leaf_version;
        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_scripts = BTreeMap::from_iter([(
            forfeit_control_block,
            (forfeit_script.clone(), leaf_version),
        )]);

        let prevouts = forfeit_psbt
            .inputs
            .iter()
            .filter_map(|i| i.witness_utxo.clone())
            .collect::<Vec<_>>();
        let prevouts = Prevouts::All(&prevouts);

        let leaf_hash = TapLeafHash::from_script(&forfeit_script, leaf_version);

        let tap_sighash = SighashCache::new(&forfeit_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(
                FORFEIT_TX_VTXO_INDEX,
                &prevouts,
                leaf_hash,
                TapSighashType::Default,
            )
            .map_err(Error::crypto)?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let (sig, pk) = sign(&msg, vtxo);

        secp.verify_schnorr(&sig, &msg, &pk)
            .map_err(Error::crypto)
            .context("failed to verify own forfeit signature")?;

        let sig = taproot::Signature {
            signature: sig,
            sighash_type: TapSighashType::Default,
        };

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_script_sigs =
            BTreeMap::from_iter([((pk, leaf_hash), sig)]);

        signed_forfeit_psbts.push(forfeit_psbt.clone());
    }

    Ok(signed_forfeit_psbts)
}

/// Sign every input of the `commitment_psbt` which is in the provided `onchain_inputs` list.
pub fn sign_commitment_psbt<F>(
    sign_for_pk_fn: F,
    commitment_psbt: &mut Psbt,
    onchain_inputs: &[OnChainInput],
) -> Result<(), Error>
where
    F: Fn(&XOnlyPublicKey, &secp256k1::Message) -> Result<schnorr::Signature, Error>,
{
    let secp = Secp256k1::new();

    let prevouts = commitment_psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Sign commitment transaction inputs that belong to us. For every output we are settling, we
    // look through the commitment transaction inputs to find a matching input.
    for OnChainInput {
        boarding_output,
        outpoint: boarding_outpoint,
        ..
    } in onchain_inputs.iter()
    {
        let (forfeit_script, forfeit_control_block) = boarding_output.forfeit_spend_info();

        for (i, input) in commitment_psbt.inputs.iter_mut().enumerate() {
            let previous_outpoint = commitment_psbt.unsigned_tx.input[i].previous_output;

            if previous_outpoint == *boarding_outpoint {
                // In the case of a boarding output, we are actually using a
                // script spend path.

                let leaf_version = forfeit_control_block.leaf_version;
                input.tap_scripts = BTreeMap::from_iter([(
                    forfeit_control_block.clone(),
                    (forfeit_script.clone(), leaf_version),
                )]);

                let prevouts = Prevouts::All(&prevouts);

                let leaf_hash = TapLeafHash::from_script(&forfeit_script, leaf_version);

                let tap_sighash = SighashCache::new(&commitment_psbt.unsigned_tx)
                    .taproot_script_spend_signature_hash(
                        i,
                        &prevouts,
                        leaf_hash,
                        TapSighashType::Default,
                    )
                    .map_err(Error::crypto)?;

                let msg =
                    secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());
                let pk = boarding_output.owner_pk();

                let sig = sign_for_pk_fn(&pk, &msg)?;

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify own commitment TX signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);
            }
        }
    }

    Ok(())
}

/// Build a map between VTXOs and their corresponding connector outputs.
fn derive_vtxo_connector_map(
    vtxo_inputs: &[VtxoInput],
    connectors_leaves: &[&Psbt],
    dust: Amount,
) -> Result<HashMap<OutPoint, OutPoint>, Error> {
    // Collect all connector outpoints (non-anchor outputs).
    let mut connector_outpoints = Vec::new();
    for psbt in connectors_leaves.iter() {
        for (vout, output) in psbt.unsigned_tx.output.iter().enumerate() {
            // Skip anchor outputs.
            if output.value == Amount::ZERO {
                continue;
            }
            connector_outpoints.push(OutPoint {
                txid: psbt.unsigned_tx.compute_txid(),
                vout: vout as u32,
            });
        }
    }

    // Sort connector outpoints for deterministic ordering
    connector_outpoints.sort_by(|a, b| a.txid.cmp(&b.txid).then(a.vout.cmp(&b.vout)));

    // Get virtual TX outpoints that need forfeiting (excluding sub-dust ones).
    let mut virtual_tx_outpoints = vtxo_inputs
        .iter()
        .filter_map(|vtxo_input| (vtxo_input.amount > dust).then_some(vtxo_input.outpoint))
        .collect::<Vec<_>>();

    // Sort virtual TX outpoints for deterministic ordering.
    virtual_tx_outpoints.sort_by(|a, b| a.txid.cmp(&b.txid).then(a.vout.cmp(&b.vout)));

    // Ensure we have matching counts.
    if virtual_tx_outpoints.len() != connector_outpoints.len() {
        return Err(Error::ad_hoc(format!(
            "mismatch between VTXO count ({}) and connector count ({})",
            virtual_tx_outpoints.len(),
            connector_outpoints.len()
        )));
    }

    // Create mapping by position.
    let mut map = HashMap::new();
    for (virtual_tx_outpoint, connector_outpoint) in
        virtual_tx_outpoints.iter().zip(connector_outpoints.iter())
    {
        map.insert(*virtual_tx_outpoint, *connector_outpoint);
    }

    Ok(map)
}

fn extract_cosigner_pks_from_vtxo_psbt(psbt: &Psbt) -> Result<Vec<PublicKey>, Error> {
    let vtxo_input = &psbt.inputs[VTXO_INPUT_INDEX];

    let mut cosigner_pks = Vec::new();
    for (key, pk) in vtxo_input.unknown.iter() {
        if key.key.starts_with(&VTXO_COSIGNER_PSBT_KEY) {
            cosigner_pks.push(
                bitcoin::PublicKey::from_slice(pk)
                    .map_err(Error::crypto)
                    .context("invalid PK")?
                    .inner,
            );
        }
    }
    Ok(cosigner_pks)
}

// =============================================================================
// Delegation API
// =============================================================================

/// Contains all the PSBTs and metadata needed for delegation settlement.
///
/// This struct supports a three-party flow where:
/// 1. Bob prepares the unsigned PSBTs
/// 2. Alice signs the PSBTs
/// 3. Bob completes the settlement using Alice's signatures
#[derive(Debug, Clone)]
pub struct DelegationPsbts {
    /// The unsigned intent PSBT
    pub intent_psbt: Psbt,
    /// The intent message to be registered
    pub intent_message: crate::intent::IntentMessage,
    /// Partial forfeit PSBTs (one per non-recoverable VTXO input)
    pub forfeit_psbts: Vec<Psbt>,
    /// Intent inputs (needed for signing)
    pub intent_inputs: Vec<crate::intent::Input>,
    /// VTXO inputs being delegated
    pub vtxo_inputs: Vec<VtxoInput>,
    /// Outputs for the settlement
    pub outputs: Vec<crate::intent::Output>,
}

/// Prepare unsigned intent and forfeit PSBTs for delegation.
///
/// This is step 1 of the delegation flow. Bob can prepare these PSBTs and send them to Alice
/// for signing.
///
/// # Arguments
///
/// * `vtxo_inputs` - VTXO inputs to be settled
/// * `onchain_inputs` - Onchain inputs to be settled
/// * `outputs` - Desired outputs (typically back to the owner's address)
/// * `cosigner_pks` - Public keys of cosigners who will participate in the settlement
/// * `server_forfeit_address` - Address where forfeits are sent
/// * `dust` - Dust amount for connectors
///
/// # Returns
///
/// A [`DelegationPsbts`] struct containing unsigned PSBTs ready for signing.
pub fn prepare_delegation_psbts(
    vtxo_inputs: Vec<VtxoInput>,
    onchain_inputs: Vec<OnChainInput>,
    outputs: Vec<crate::intent::Output>,
    cosigner_pks: Vec<PublicKey>,
    server_forfeit_address: &Address,
    dust: Amount,
) -> Result<DelegationPsbts, Error> {
    use crate::intent;
    use bitcoin::psbt::PsbtSighashType;

    // Convert inputs to intent::Input format
    let intent_inputs = {
        let boarding_inputs = onchain_inputs.iter().map(|o| {
            intent::Input::new(
                o.outpoint(),
                o.boarding_output().exit_delay(),
                TxOut {
                    value: o.amount(),
                    script_pubkey: o.boarding_output().script_pubkey(),
                },
                o.boarding_output().tapscripts(),
                o.boarding_output().owner_pk(),
                o.boarding_output().forfeit_spend_info(),
                true,
            )
        });

        let vtxo_inputs_iter = vtxo_inputs
            .iter()
            .map(|v| {
                Ok(intent::Input::new(
                    v.outpoint(),
                    v.vtxo().exit_delay(),
                    TxOut {
                        value: v.amount(),
                        script_pubkey: v.vtxo().script_pubkey(),
                    },
                    v.vtxo().tapscripts(),
                    v.vtxo().owner_pk(),
                    v.vtxo()
                        .forfeit_spend_info()
                        .context("failed to get forfeit spend info")?,
                    false,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        boarding_inputs.chain(vtxo_inputs_iter).collect::<Vec<_>>()
    };

    // Determine onchain output indexes
    let mut onchain_output_indexes = Vec::new();
    for (i, output) in outputs.iter().enumerate() {
        if matches!(output, intent::Output::Onchain(_)) {
            onchain_output_indexes.push(i);
        }
    }

    // Create intent message
    let now = std::time::SystemTime::now();
    let now = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(Error::ad_hoc)
        .context("failed to compute now timestamp")?;
    let now = now.as_secs();
    let expire_at = now + (2 * 60);

    let intent_message = intent::IntentMessage::new(
        intent::IntentMessageType::Register,
        onchain_output_indexes,
        now,
        expire_at,
        cosigner_pks.clone(),
    );

    // Build the intent PSBT (unsigned)
    let (intent_psbt, _fake_input) =
        intent::build_proof_psbt(&intent_message, &intent_inputs, &outputs)?;

    // Build unsigned forfeit PSBTs
    let mut forfeit_psbts = Vec::new();
    const FORFEIT_TX_VTXO_INDEX: usize = 0;

    for vtxo_input in vtxo_inputs.iter() {
        let vtxo = vtxo_input.vtxo();
        let vtxo_amount = vtxo_input.amount();
        let virtual_tx_outpoint = vtxo_input.outpoint();
        let connector_amount = dust;

        // Create partial forfeit transaction with only the VTXO input
        let forfeit_output = TxOut {
            value: vtxo_amount + connector_amount,
            script_pubkey: server_forfeit_address.script_pubkey(),
        };

        let mut forfeit_psbt = Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: virtual_tx_outpoint,
                ..Default::default()
            }],
            output: vec![forfeit_output, anchor_output()],
        })
        .map_err(|e| Error::ad_hoc(format!("failed to create forfeit PSBT: {e}")))?;

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].witness_utxo = Some(TxOut {
            value: vtxo_amount,
            script_pubkey: vtxo.script_pubkey(),
        });

        // Set sighash type to SIGHASH_ALL | ANYONECANPAY
        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].sighash_type =
            Some(PsbtSighashType::from(TapSighashType::AllPlusAnyoneCanPay));

        let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info()?;
        let leaf_version = forfeit_control_block.leaf_version;
        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_scripts =
            BTreeMap::from_iter([(forfeit_control_block, (forfeit_script, leaf_version))]);

        forfeit_psbts.push(forfeit_psbt);
    }

    Ok(DelegationPsbts {
        intent_psbt,
        intent_message,
        forfeit_psbts,
        intent_inputs,
        vtxo_inputs,
        outputs,
    })
}

/// Prepare delegation PSBTs for shared VTXOs (2-of-2 or multi-sig).
///
/// This creates unsigned intent and forfeit PSBTs that can be signed by multiple parties
/// in sequence. The settler (the party who will perform the batch protocol) provides their
/// cosigner public key which will be used for tree signing during settlement.
///
/// # Arguments
///
/// * `vtxo_inputs` - VTXOs to delegate (may have multiple owners)
/// * `onchain_inputs` - Onchain boarding inputs (if any)
/// * `outputs` - Flexible outputs (can go to any party or combination)
/// * `settler_cosigner_pk` - The cosigner public key of the party who will settle (do tree signing)
/// * `server_forfeit_address` - Server's forfeit address
/// * `dust` - Dust amount for connector outputs
///
/// # Returns
///
/// Unsigned `DelegationPsbts` that can be signed by each party using `sign_shared_delegation_psbts`
pub fn prepare_shared_delegation_psbts(
    vtxo_inputs: Vec<VtxoInput>,
    outputs: Vec<crate::intent::Output>,
    settler_cosigner_pk: PublicKey,
    server_forfeit_address: &Address,
    dust: Amount,
) -> Result<DelegationPsbts, Error> {
    use crate::intent;
    use bitcoin::psbt::PsbtSighashType;

    // Convert inputs to intent::Input format
    let intent_inputs = vtxo_inputs
        .iter()
        .map(|v| {
            Ok(intent::Input::new(
                v.outpoint(),
                v.vtxo().exit_delay(),
                TxOut {
                    value: v.amount(),
                    script_pubkey: v.vtxo().script_pubkey(),
                },
                v.vtxo().tapscripts(),
                v.vtxo().owner_pk(),
                v.vtxo()
                    .forfeit_spend_info()
                    .context("failed to get forfeit spend info")?,
                false,
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;

    // Create intent message with only the settler's cosigner PK
    let now = std::time::SystemTime::now();
    let now = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(Error::ad_hoc)
        .context("failed to compute now timestamp")?;
    let now = now.as_secs();
    let expire_at = now + (2 * 60);

    let intent_message = intent::IntentMessage::new(
        intent::IntentMessageType::Register,
        Vec::new(),
        now,
        expire_at,
        vec![settler_cosigner_pk],
    );

    // Build the intent PSBT (unsigned)
    let (intent_psbt, _fake_input) =
        intent::build_proof_psbt(&intent_message, &intent_inputs, &outputs)?;

    // Build unsigned forfeit PSBTs
    let mut forfeit_psbts = Vec::new();
    const FORFEIT_TX_VTXO_INDEX: usize = 0;

    for vtxo_input in vtxo_inputs.iter() {
        let vtxo = vtxo_input.vtxo();
        let vtxo_amount = vtxo_input.amount();
        let virtual_tx_outpoint = vtxo_input.outpoint();
        let connector_amount = dust;

        // Create partial forfeit transaction with only the VTXO input
        let forfeit_output = TxOut {
            value: vtxo_amount + connector_amount,
            script_pubkey: server_forfeit_address.script_pubkey(),
        };

        let mut forfeit_psbt = Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::non_standard(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: virtual_tx_outpoint,
                ..Default::default()
            }],
            output: vec![forfeit_output, anchor_output()],
        })
        .map_err(|e| Error::ad_hoc(format!("failed to create forfeit PSBT: {e}")))?;

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].witness_utxo = Some(TxOut {
            value: vtxo_amount,
            script_pubkey: vtxo.script_pubkey(),
        });

        // Set sighash type to SIGHASH_ALL | ANYONECANPAY
        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].sighash_type =
            Some(PsbtSighashType::from(TapSighashType::AllPlusAnyoneCanPay));

        let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info()?;
        let leaf_version = forfeit_control_block.leaf_version;
        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_scripts =
            BTreeMap::from_iter([(forfeit_control_block, (forfeit_script, leaf_version))]);

        forfeit_psbts.push(forfeit_psbt);
    }

    Ok(DelegationPsbts {
        intent_psbt,
        intent_message,
        forfeit_psbts,
        intent_inputs,
        vtxo_inputs,
        outputs,
    })
}

/// Sign delegation PSBTs with a keypair.
///
/// This is step 2 of the delegation flow. Alice receives the unsigned PSBTs from Bob, signs them,
/// and returns them.
///
/// # Arguments
///
/// * `delegation_psbts` - The unsigned PSBTs prepared by Bob
/// * `signing_kps` - Keypairs to sign with (for VTXO inputs)
/// * `sign_for_onchain_pk_fn` - Function to sign for onchain inputs (e.g., boarding outputs)
///
/// # Errors
///
/// Returns an error if signing fails.
pub fn sign_delegation_psbts<F>(
    delegation_psbts: &mut DelegationPsbts,
    signing_kps: &[Keypair],
    sign_for_onchain_pk_fn: F,
) -> Result<(), Error>
where
    F: Fn(&XOnlyPublicKey, &secp256k1::Message) -> Result<schnorr::Signature, Error>,
{
    use crate::intent;
    use bitcoin::psbt;

    let secp = Secp256k1::new();

    // Sign the intent PSBT
    for (i, proof_input) in delegation_psbts.intent_psbt.inputs.iter_mut().enumerate() {
        if i == 0 {
            let (script, control_block) = delegation_psbts.intent_inputs[0].spend_info().clone();

            proof_input.tap_scripts =
                BTreeMap::from_iter([(control_block, (script, taproot::LeafVersion::TapScript))]);
        } else {
            let (script, control_block) =
                delegation_psbts.intent_inputs[i - 1].spend_info().clone();

            let tap_tree = crate::intent::taptree::TapTree(
                delegation_psbts.intent_inputs[i - 1].tapscripts().to_vec(),
            );
            let bytes = tap_tree
                .encode()
                .map_err(Error::ad_hoc)
                .with_context(|| format!("failed to encode taptree for input {i}"))?;

            proof_input.unknown.insert(
                psbt::raw::Key {
                    type_value: 222,
                    key: crate::VTXO_TAPROOT_KEY.to_vec(),
                },
                bytes,
            );
            proof_input.tap_scripts =
                BTreeMap::from_iter([(control_block, (script, taproot::LeafVersion::TapScript))]);
        };
    }

    let prevouts = delegation_psbts
        .intent_psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Add fake input to the inputs list for signing
    let inputs = [
        delegation_psbts.intent_inputs.clone(),
        vec![intent::Input::new(
            OutPoint {
                txid: delegation_psbts.intent_psbt.unsigned_tx.input[0]
                    .previous_output
                    .txid,
                vout: 0,
            },
            bitcoin::Sequence::ZERO,
            TxOut {
                value: Amount::ZERO,
                script_pubkey: prevouts[0].script_pubkey.clone(),
            },
            Vec::new(),
            delegation_psbts.intent_inputs[0].pk(),
            delegation_psbts.intent_inputs[0].spend_info().clone(),
            false,
        )],
    ]
    .concat();

    for (i, proof_input) in delegation_psbts.intent_psbt.inputs.iter_mut().enumerate() {
        let input = inputs
            .iter()
            .find(|input| {
                input.outpoint()
                    == delegation_psbts.intent_psbt.unsigned_tx.input[i].previous_output
            })
            .expect("witness utxo");

        let prevouts = Prevouts::All(&prevouts);

        let (_, (script, leaf_version)) =
            proof_input.tap_scripts.first_key_value().expect("a value");

        let leaf_hash = TapLeafHash::from_script(script, *leaf_version);

        let tap_sighash = SighashCache::new(&delegation_psbts.intent_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(i, &prevouts, leaf_hash, TapSighashType::Default)
            .map_err(Error::crypto)
            .with_context(|| format!("failed to compute sighash for intent input {i}"))?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let pk = input.pk();

        match input.is_onchain() {
            true => {
                let sig = sign_for_onchain_pk_fn(&pk, &msg)?;

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify intent boarding output signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                proof_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);
            }
            false => {
                let signing_kp = signing_kps
                    .iter()
                    .find(|kp| {
                        let (xonly_pk, _) = kp.x_only_public_key();
                        xonly_pk == pk
                    })
                    .ok_or_else(|| Error::ad_hoc("Could not find suitable kp for pk"))?;

                let sig = secp.sign_schnorr_no_aux_rand(&msg, signing_kp);

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify intent vtxo signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                proof_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);
            }
        };
    }

    // Sign the forfeit PSBTs
    const FORFEIT_TX_VTXO_INDEX: usize = 0;

    for (forfeit_psbt, vtxo_input) in delegation_psbts
        .forfeit_psbts
        .iter_mut()
        .zip(delegation_psbts.vtxo_inputs.iter())
    {
        let vtxo = vtxo_input.vtxo();

        let prevouts = forfeit_psbt
            .inputs
            .iter()
            .filter_map(|i| i.witness_utxo.clone())
            .collect::<Vec<_>>();
        let prevouts = Prevouts::All(&prevouts);

        let (forfeit_script, _) = vtxo.forfeit_spend_info()?;
        let (_, (_, leaf_version)) = forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX]
            .tap_scripts
            .first_key_value()
            .expect("tap scripts");
        let leaf_hash = TapLeafHash::from_script(&forfeit_script, *leaf_version);

        let tap_sighash = SighashCache::new(&forfeit_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(
                FORFEIT_TX_VTXO_INDEX,
                &prevouts,
                leaf_hash,
                TapSighashType::AllPlusAnyoneCanPay,
            )
            .map_err(|e| Error::ad_hoc(format!("failed to compute forfeit sighash: {e}")))?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let pk = vtxo.owner_pk();
        let signing_kp = signing_kps
            .iter()
            .find(|kp| {
                let (xonly_pk, _) = kp.x_only_public_key();
                xonly_pk == pk
            })
            .ok_or_else(|| Error::ad_hoc("Could not find suitable kp for forfeit signing"))?;

        let sig = secp.sign_schnorr_no_aux_rand(&msg, signing_kp);

        secp.verify_schnorr(&sig, &msg, &pk)
            .map_err(|e| Error::ad_hoc(format!("failed to verify forfeit signature: {e}")))?;

        let sig = taproot::Signature {
            signature: sig,
            sighash_type: TapSighashType::AllPlusAnyoneCanPay,
        };

        forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX].tap_script_sigs =
            BTreeMap::from_iter([((pk, leaf_hash), sig)]);
    }

    Ok(())
}

/// Sign shared delegation PSBTs with the provided keypairs (accumulative).
///
/// This function can be called multiple times by different parties to accumulate signatures
/// on the same PSBTs. It will add signatures for any keys in `signing_kps` that match the
/// inputs, and skip inputs where:
/// - The key doesn't match (other party's key)
/// - A signature already exists (already signed by this or another party)
///
/// This is the signing step for shared VTXO delegation, where both Alice and Bob need to
/// sign the intent and forfeit PSBTs before Alice performs settlement.
///
/// # Arguments
///
/// * `delegation_psbts` - The PSBTs to sign (may be partially signed already)
/// * `signing_kps` - Keypairs to sign with (subset of owners)
/// * `sign_for_onchain_pk_fn` - Function to sign for onchain inputs (e.g., boarding outputs)
///
/// # Returns
///
/// Returns `Ok(())` if signing succeeded for at least one input, or if all inputs were already
/// signed. Returns an error only if signature generation/verification fails.
///
/// # Example
///
/// ```rust,ignore
/// // Bob signs first
/// sign_shared_delegation_psbts(&mut psbts, &[bob_kp], sign_fn)?;
///
/// // Alice signs second (accumulates on top of Bob's signatures)
/// sign_shared_delegation_psbts(&mut psbts, &[alice_kp], sign_fn)?;
/// ```
pub fn sign_shared_delegation_psbts(
    delegation_psbts: &mut DelegationPsbts,
    signing_kps: &[Keypair],
) -> Result<(), Error> {
    use crate::intent;
    use bitcoin::psbt;

    let secp = Secp256k1::new();

    // Sign the intent PSBT
    for (i, proof_input) in delegation_psbts.intent_psbt.inputs.iter_mut().enumerate() {
        if i == 0 {
            let (script, control_block) = delegation_psbts.intent_inputs[0].spend_info().clone();

            proof_input.tap_scripts =
                BTreeMap::from_iter([(control_block, (script, taproot::LeafVersion::TapScript))]);
        } else {
            let (script, control_block) =
                delegation_psbts.intent_inputs[i - 1].spend_info().clone();

            let tap_tree = crate::intent::taptree::TapTree(
                delegation_psbts.intent_inputs[i - 1].tapscripts().to_vec(),
            );
            let bytes = tap_tree
                .encode()
                .map_err(Error::ad_hoc)
                .with_context(|| format!("failed to encode taptree for input {i}"))?;

            proof_input.unknown.insert(
                psbt::raw::Key {
                    type_value: 222,
                    key: crate::VTXO_TAPROOT_KEY.to_vec(),
                },
                bytes,
            );
            proof_input.tap_scripts =
                BTreeMap::from_iter([(control_block, (script, taproot::LeafVersion::TapScript))]);
        };
    }

    let prevouts = delegation_psbts
        .intent_psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Add fake input to the inputs list for signing
    let inputs = [
        delegation_psbts.intent_inputs.clone(),
        vec![intent::Input::new(
            OutPoint {
                txid: delegation_psbts.intent_psbt.unsigned_tx.input[0]
                    .previous_output
                    .txid,
                vout: 0,
            },
            bitcoin::Sequence::ZERO,
            TxOut {
                value: Amount::ZERO,
                script_pubkey: prevouts[0].script_pubkey.clone(),
            },
            Vec::new(),
            delegation_psbts.intent_inputs[0].pk(),
            delegation_psbts.intent_inputs[0].spend_info().clone(),
            false,
        )],
    ]
    .concat();

    for (i, proof_input) in delegation_psbts.intent_psbt.inputs.iter_mut().enumerate() {
        let input = inputs
            .iter()
            .find(|input| {
                input.outpoint()
                    == delegation_psbts.intent_psbt.unsigned_tx.input[i].previous_output
            })
            .expect("witness utxo");

        let prevouts = Prevouts::All(&prevouts);

        let (_, (script, leaf_version)) =
            proof_input.tap_scripts.first_key_value().expect("a value");

        let leaf_hash = TapLeafHash::from_script(script, *leaf_version);

        let pk = input.pk();

        // Check if this input is already signed
        if proof_input.tap_script_sigs.contains_key(&(pk, leaf_hash)) {
            // Already signed, skip
            continue;
        }

        let tap_sighash = SighashCache::new(&delegation_psbts.intent_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(i, &prevouts, leaf_hash, TapSighashType::Default)
            .map_err(Error::crypto)
            .with_context(|| format!("failed to compute sighash for intent input {i}"))?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        // Try to find matching keypair for VTXO input
        if let Some(signing_kp) = signing_kps.iter().find(|kp| {
            let (xonly_pk, _) = kp.x_only_public_key();
            xonly_pk == pk
        }) {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, signing_kp);

            secp.verify_schnorr(&sig, &msg, &pk)
                .map_err(Error::crypto)
                .context("failed to verify intent vtxo signature")?;

            let sig = taproot::Signature {
                signature: sig,
                sighash_type: TapSighashType::Default,
            };

            proof_input.tap_script_sigs.insert((pk, leaf_hash), sig);
        }

        // If no matching keypair, skip (other party's key)
    }

    // Sign the forfeit PSBTs
    const FORFEIT_TX_VTXO_INDEX: usize = 0;

    for (forfeit_psbt, vtxo_input) in delegation_psbts
        .forfeit_psbts
        .iter_mut()
        .zip(delegation_psbts.vtxo_inputs.iter())
    {
        let vtxo = vtxo_input.vtxo();

        let prevouts = forfeit_psbt
            .inputs
            .iter()
            .filter_map(|i| i.witness_utxo.clone())
            .collect::<Vec<_>>();
        let prevouts = Prevouts::All(&prevouts);

        let (forfeit_script, _) = vtxo.forfeit_spend_info()?;
        let (_, (_, leaf_version)) = forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX]
            .tap_scripts
            .first_key_value()
            .expect("tap scripts");
        let leaf_hash = TapLeafHash::from_script(&forfeit_script, *leaf_version);

        let pk = vtxo.owner_pk();

        // Check if this input is already signed
        if forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX]
            .tap_script_sigs
            .contains_key(&(pk, leaf_hash))
        {
            // Already signed, skip
            continue;
        }

        // Try to find matching keypair
        if let Some(signing_kp) = signing_kps.iter().find(|kp| {
            let (xonly_pk, _) = kp.x_only_public_key();
            xonly_pk == pk
        }) {
            let tap_sighash = SighashCache::new(&forfeit_psbt.unsigned_tx)
                .taproot_script_spend_signature_hash(
                    FORFEIT_TX_VTXO_INDEX,
                    &prevouts,
                    leaf_hash,
                    TapSighashType::AllPlusAnyoneCanPay,
                )
                .map_err(|e| Error::ad_hoc(format!("failed to compute forfeit sighash: {e}")))?;

            let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

            let sig = secp.sign_schnorr_no_aux_rand(&msg, signing_kp);

            secp.verify_schnorr(&sig, &msg, &pk)
                .map_err(|e| Error::ad_hoc(format!("failed to verify forfeit signature: {e}")))?;

            let sig = taproot::Signature {
                signature: sig,
                sighash_type: TapSighashType::AllPlusAnyoneCanPay,
            };

            forfeit_psbt.inputs[FORFEIT_TX_VTXO_INDEX]
                .tap_script_sigs
                .insert((pk, leaf_hash), sig);
        }
        // If no matching keypair, skip (other party's key)
    }

    Ok(())
}
