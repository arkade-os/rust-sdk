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
    is_recoverable: bool,
}

impl VtxoInput {
    pub fn new(vtxo: Vtxo, amount: Amount, outpoint: OutPoint, is_recoverable: bool) -> Self {
        Self {
            vtxo,
            amount,
            outpoint,
            is_recoverable,
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
    pub fn get_pk(&mut self, txid: &Txid) -> Option<musig::PublicNonce> {
        self.0.get(txid).map(|(_, pk)| pk).copied()
    }

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

    let connector_index = derive_vtxo_connector_map(vtxo_inputs, connectors_leaves)?;

    let mut signed_forfeit_psbts = Vec::new();
    for VtxoInput {
        vtxo,
        amount: vtxo_amount,
        outpoint: virtual_tx_outpoint,
        is_recoverable,
    } in vtxo_inputs.iter()
    {
        if *is_recoverable {
            // Recoverable VTXOs don't need to be forfeited.
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

    // Get virtual TX outpoints that need forfeiting (excluding recoverable ones).
    let mut virtual_tx_outpoints = Vec::new();
    for vtxo_input in vtxo_inputs.iter() {
        if !vtxo_input.is_recoverable {
            virtual_tx_outpoints.push(vtxo_input.outpoint);
        }
    }

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
