use crate::anchor_output;
use crate::script::extract_checksig_pubkeys;
use crate::server;
use crate::BoardingOutput;
use crate::Error;
use crate::ErrorContext;
use crate::VTXO_CONDITION_KEY;
use crate::VTXO_INPUT_INDEX;
use bitcoin::absolute::LockTime;
use bitcoin::consensus::Decodable;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::transaction;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::VarInt;
use bitcoin::Weight;
use bitcoin::Witness;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::collections::HashSet;

/// A UTXO that could have become a VTXO with the help of the Ark server, but is now unilaterally
/// spendable by the original owner.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OnChainInput {
    /// The information needed to spend the UTXO, besides the amount.
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

    pub fn previous_output(&self) -> TxOut {
        TxOut {
            value: self.amount,
            script_pubkey: self.boarding_output.script_pubkey(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VtxoInput {
    outpoint: OutPoint,
    sequence: Sequence,
    witness_utxo: TxOut,
    /// Where the VTXO would end up on the blockchain if it were to become a UTXO.
    spend_info: (ScriptBuf, taproot::ControlBlock),
}

impl VtxoInput {
    pub fn new(
        outpoint: OutPoint,
        sequence: Sequence,
        witness_utxo: TxOut,
        spend_info: (ScriptBuf, taproot::ControlBlock),
    ) -> Self {
        Self {
            outpoint,
            sequence,
            witness_utxo,
            spend_info,
        }
    }

    pub fn previous_output(&self) -> TxOut {
        self.witness_utxo.clone()
    }
}

/// Build a transaction that spends boarding outputs and VTXOs to an _on-chain_ `to_address`. Any
/// coins left over after covering the `to_amount` are sent to an on-chain change address.
///
/// All these outputs are spent unilaterally i.e. without the collaboration of the Ark server.
///
/// To be able to spend a boarding output, we must wait for the exit delay to pass.
///
/// To be able to spend a VTXO, the VTXO itself must be published on-chain, and then we must wait
/// for the exit delay to pass.
pub fn create_unilateral_exit_transaction<S>(
    to_address: Address,
    to_amount: Amount,
    change_address: Address,
    onchain_inputs: &[OnChainInput],
    vtxo_inputs: &[VtxoInput],
    sign_fn: S,
) -> Result<Transaction, Error>
where
    S: Fn(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, Error>,
{
    if onchain_inputs.is_empty() && vtxo_inputs.is_empty() {
        return Err(Error::transaction(
            "cannot create transaction without inputs",
        ));
    }

    let secp = Secp256k1::new();

    let mut output = vec![TxOut {
        value: to_amount,
        script_pubkey: to_address.script_pubkey(),
    }];

    let total_amount: Amount = onchain_inputs
        .iter()
        .map(|o| o.amount)
        .chain(vtxo_inputs.iter().map(|v| v.witness_utxo.value))
        .sum();

    let change_amount = total_amount.checked_sub(to_amount).ok_or_else(|| {
        Error::transaction(format!(
            "cannot cover to_amount ({to_amount}) with total input amount ({total_amount})"
        ))
    })?;

    if change_amount > Amount::ZERO {
        output.push(TxOut {
            value: change_amount,
            script_pubkey: change_address.script_pubkey(),
        });
    }

    let input = {
        let onchain_inputs = onchain_inputs.iter().map(|o| TxIn {
            previous_output: o.outpoint,
            sequence: o.boarding_output.exit_delay(),
            ..Default::default()
        });

        let vtxo_inputs = vtxo_inputs.iter().map(|v| TxIn {
            previous_output: v.outpoint,
            sequence: v.sequence,
            ..Default::default()
        });

        onchain_inputs.chain(vtxo_inputs).collect::<Vec<_>>()
    };

    let mut psbt = Psbt::from_unsigned_tx(Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input,
        output,
    })
    .map_err(Error::transaction)?;

    // Add a `witness_utxo` for every transaction input.
    for (i, input) in psbt.inputs.iter_mut().enumerate() {
        let outpoint = psbt.unsigned_tx.input[i].previous_output;

        for onchain_input in onchain_inputs {
            if onchain_input.outpoint == outpoint {
                input.witness_utxo = Some(TxOut {
                    value: onchain_input.amount,
                    script_pubkey: onchain_input.boarding_output.address().script_pubkey(),
                });

                let (script, cb) = onchain_input.boarding_output.exit_spend_info();
                let leaf_version = cb.leaf_version;
                input.tap_scripts.insert(cb, (script, leaf_version));
            }
        }

        for vtxo_input in vtxo_inputs.iter() {
            if vtxo_input.outpoint == outpoint {
                input.witness_utxo = Some(TxOut {
                    value: vtxo_input.witness_utxo.value,
                    script_pubkey: vtxo_input.witness_utxo.script_pubkey.clone(),
                });

                let (script, cb) = vtxo_input.spend_info.clone();
                let leaf_version = cb.leaf_version;
                input.tap_scripts.insert(cb, (script, leaf_version));
            }
        }
    }

    // Collect all `witness_utxo` entries.
    let prevouts = psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Sign each input.
    for (i, input) in psbt.inputs.iter_mut().enumerate() {
        let (exit_control_block, (exit_script, leaf_version)) = input
            .tap_scripts
            .pop_first()
            .ok_or_else(|| Error::ad_hoc(format!("no exit script found for input {i}")))?;

        input.witness_script = Some(exit_script.clone());

        let leaf_hash = TapLeafHash::from_script(&exit_script, leaf_version);

        let tap_sighash = SighashCache::new(&psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(
                i,
                &Prevouts::All(&prevouts),
                leaf_hash,
                TapSighashType::Default,
            )
            .map_err(Error::crypto)?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let sigs = sign_fn(input, msg)?;

        let mut witness = Vec::new();
        for (sig, pk) in sigs.iter() {
            secp.verify_schnorr(sig, &msg, pk)
                .map_err(Error::crypto)
                .with_context(|| format!("failed to verify own signature for input {i}"))?;

            witness.push(&sig[..]);
        }

        witness.push(exit_script.as_bytes());

        let control_block = exit_control_block.serialize();
        witness.push(control_block.as_slice());

        let witness = Witness::from_slice(&witness);

        input.final_script_witness = Some(witness);
    }

    let tx = psbt.clone().extract_tx().map_err(Error::transaction)?;

    tracing::debug!(
        ?onchain_inputs,
        ?vtxo_inputs,
        raw_tx = %bitcoin::consensus::serialize(&tx).as_hex(),
        "Built transaction sending inputs to on-chain address"
    );

    Ok(tx)
}

/// Build the unilateral exit tree of TXIDs for a VTXO from a [`server::VtxoChains`].
pub fn build_unilateral_exit_tree_txids(
    vtxo_chains: &server::VtxoChains,
    // The TXID of the VTXO we want to commit on-chain.
    ark_txid: Txid,
) -> Result<Vec<Vec<Txid>>, Error> {
    // Create a hash-map for quick lookups: TXID -> `VtxoChain`.
    let mut chain_map: HashMap<Txid, &server::VtxoChain> = HashMap::new();
    for vtxo_chain in &vtxo_chains.inner {
        chain_map.insert(vtxo_chain.txid, vtxo_chain);
    }

    /// Find all the paths from a virtual transaction to the root commitment transaction,
    /// recursively.
    fn find_paths_to_commitment(
        current_txid: Txid,
        chain_map: &HashMap<Txid, &server::VtxoChain>,
        current_path: &mut Vec<Txid>,
        all_paths: &mut Vec<Vec<Txid>>,
        visited: &mut HashSet<Txid>,
    ) -> Result<(), Error> {
        // Safety check to prevent an infinite loop.
        if current_path.len() > 1_000 {
            return Err(Error::ad_hoc(
                "chain traversal exceeded maximum depth of 1000",
            ));
        }

        // Safety check to reject cycles.
        if visited.contains(&current_txid) {
            return Err(Error::ad_hoc("chain traversal led to cycle"));
        }
        visited.insert(current_txid);

        // Add current TXID to path.
        current_path.push(current_txid);

        // Look through parent transactions to continue building up the chain(s).
        let chain = chain_map.get(&current_txid).ok_or_else(|| {
            Error::ad_hoc(format!("could not find VtxoChain for TXID: {current_txid}",))
        })?;
        // Check if any of the transactions spent by this virtual TX are the commitment transaction.
        let mut reached_commitment = false;

        for &parent_txid in &chain.spends {
            // Look up the parent transaction's chain to get its type
            let parent_chain = chain_map.get(&parent_txid).ok_or_else(|| {
                Error::ad_hoc(format!(
                    "could not find VtxoChain for parent TXID: {parent_txid}",
                ))
            })?;

            match parent_chain.tx_type {
                server::ChainedTxType::Commitment => {
                    // We've reached our destination.
                    all_paths.push(current_path.clone());

                    reached_commitment = true;
                }
                server::ChainedTxType::Ark
                | server::ChainedTxType::Checkpoint
                | server::ChainedTxType::Tree => {
                    // Continue traversing virtual transactions up the tree.
                    find_paths_to_commitment(
                        parent_txid,
                        chain_map,
                        current_path,
                        all_paths,
                        visited,
                    )?;
                }
                server::ChainedTxType::Unspecified => {
                    tracing::warn!(
                        txid = %parent_txid,
                        "Found unspecified TX type when walking up virtual TX tree. \
                         Treating it like a virtual TX"
                    );

                    // Continue traversing virtual transactions up the tree.
                    find_paths_to_commitment(
                        parent_txid,
                        chain_map,
                        current_path,
                        all_paths,
                        visited,
                    )?;
                }
            }
        }

        if !reached_commitment && chain.spends.is_empty() {
            return Err(Error::ad_hoc(format!(
                "dead end reached at TXID {current_txid} with no commitment transaction"
            )));
        }

        visited.remove(&current_txid);
        current_path.pop();
        Ok(())
    }

    let mut all_paths = Vec::new();
    let mut current_path = Vec::new();
    let mut visited = HashSet::new();

    find_paths_to_commitment(
        ark_txid,
        &chain_map,
        &mut current_path,
        &mut all_paths,
        &mut visited,
    )?;

    if all_paths.is_empty() {
        return Err(Error::ad_hoc(format!(
            "no paths found from Ark TX {ark_txid} to commitment transaction",
        )));
    }

    // Reverse each path so they go from root commitment TX to VTXO.
    let all_paths: Vec<Vec<Txid>> = all_paths
        .into_iter()
        .map(|mut path| {
            path.reverse();
            path
        })
        .collect();

    Ok(all_paths)
}

/// The full path from a commitment transaction to a VTXO. The entire path must be published
/// on-chain to execute a unilateral exit with this VTXO.
///
/// A branch may contain both batch-tree internal node transactions, which spend their parent via
/// key path, and VTXO spend transactions, which spend a confirmed or pre-confirmed VTXO via script
/// path. We use the word "tree" because a VTXO may come from more than one path, e.g. if its
/// corresponding Ark transaction has more than one input.
pub struct UnilateralExitTree {
    /// The commitment transactions from which this VTXO comes from.
    ///
    /// A pre-confirmed VTXO can have ancestors from more than one batch, hence the list.
    commitment_txids: Vec<Txid>,
    /// The chains of virtual transactions that lead to a VTXO.
    ///
    /// Virtual TXs in a branch are ordered by distance to the root commitment transaction, with
    /// virtual TXs closest to it appearing first.
    inner: Vec<Vec<Psbt>>,
}

impl UnilateralExitTree {
    pub fn new(commitment_txids: Vec<Txid>, virtual_tx_tree: Vec<Vec<Psbt>>) -> Self {
        Self {
            commitment_txids,
            inner: virtual_tx_tree,
        }
    }

    pub fn inner(&self) -> &Vec<Vec<Psbt>> {
        &self.inner
    }

    pub fn commitment_txids(&self) -> &[Txid] {
        &self.commitment_txids
    }
}

/// Finalize a virtual transaction input using only the authorization data already present in the
/// PSBT input.
///
/// This is intended for historical virtual transactions in a unilateral-exit branch. The caller
/// provides the `witness_utxo` for the input being finalized, and this function materializes either
/// the taproot key-spend witness used by batch-tree internal nodes or a satisfiable taproot
/// script-spend witness used when spending VTXOs.
pub fn finalize_virtual_tx_input(
    mut psbt: Psbt,
    input_index: usize,
    witness_utxo: TxOut,
) -> Result<Transaction, Error> {
    let input = psbt
        .inputs
        .get_mut(input_index)
        .ok_or_else(|| Error::transaction(format!("missing PSBT input {input_index}")))?;

    input.witness_utxo = Some(witness_utxo);

    let txid = psbt.unsigned_tx.compute_txid();

    if let Some(tap_key_sig) = input.tap_key_sig {
        tracing::debug!(%txid, "Finalizing batch-tree internal node key spend");

        input.final_script_witness = Some(Witness::p2tr_key_spend(&tap_key_sig));
    } else {
        tracing::debug!(%txid, "Finalizing VTXO script spend");

        input.final_script_witness = Some(finalize_taproot_script_spend_witness(input)?);
    }

    psbt.extract_tx().map_err(Error::transaction)
}

/// Build the final witness for a taproot script-spend input from its PSBT data.
///
/// The selected tapleaf is the first tap script for which signatures are available for every
/// `CHECKSIG`/`CHECKSIGVERIFY` pubkey in the script. Signatures are pushed in reverse script order.
/// Extra condition witness elements, such as VHTLC preimages, are read from the
/// `VTXO_CONDITION_KEY` unknown input field and pushed after signatures.
pub fn finalize_taproot_script_spend_witness(input: &psbt::Input) -> Result<Witness, Error> {
    for (control_block, (script, leaf_version)) in input.tap_scripts.iter() {
        let leaf_hash = TapLeafHash::from_script(script, *leaf_version);
        let pubkeys = extract_checksig_pubkeys(script);

        if pubkeys.is_empty() {
            continue;
        }

        let signatures = pubkeys
            .iter()
            .map(|pk| {
                input
                    .tap_script_sigs
                    .get(&(*pk, leaf_hash))
                    .map(|sig| sig.to_vec())
            })
            .collect::<Option<Vec<_>>>();

        let Some(signatures) = signatures else {
            continue;
        };

        let mut witness = Witness::new();

        for signature in signatures.into_iter().rev() {
            witness.push(signature);
        }

        for element in condition_witness_elements(input)? {
            witness.push(element);
        }

        witness.push(script.as_bytes());
        witness.push(control_block.serialize());

        return Ok(witness);
    }

    Err(Error::transaction(
        "no satisfiable taproot script-spend leaf found in PSBT input",
    ))
}

fn condition_witness_elements(input: &psbt::Input) -> Result<Vec<Vec<u8>>, Error> {
    let condition_key = psbt::raw::Key {
        type_value: 222,
        key: VTXO_CONDITION_KEY.to_vec(),
    };

    let Some(condition_data) = input.unknown.get(&condition_key) else {
        return Ok(Vec::new());
    };

    let mut cursor = std::io::Cursor::new(condition_data);
    let element_count = VarInt::consensus_decode(&mut cursor)
        .map_err(|e| Error::transaction(format!("failed to decode condition count: {e}")))?
        .0;

    let count_end = usize::try_from(cursor.position())
        .map_err(|_| Error::transaction("condition cursor position overflow"))?;
    let remaining_after_count = condition_data.len().saturating_sub(count_end);
    let element_count = usize::try_from(element_count)
        .map_err(|_| Error::transaction("condition witness element count overflow"))?;

    // Each element needs at least a compact-size length byte, even when the element itself is
    // empty.
    if element_count > remaining_after_count {
        return Err(Error::transaction(format!(
            "condition witness element count {element_count} exceeds remaining buffer size {remaining_after_count}"
        )));
    }

    let mut elements = Vec::with_capacity(element_count);
    for _ in 0..element_count {
        let element_len = VarInt::consensus_decode(&mut cursor)
            .map_err(|e| Error::transaction(format!("failed to decode condition length: {e}")))?
            .0;
        let element_len = usize::try_from(element_len)
            .map_err(|_| Error::transaction("condition witness element length overflow"))?;
        let start = usize::try_from(cursor.position())
            .map_err(|_| Error::transaction("condition cursor position overflow"))?;
        let end = start
            .checked_add(element_len)
            .ok_or_else(|| Error::transaction("condition witness element end overflow"))?;

        if condition_data.len() < end {
            return Err(Error::transaction(format!(
                "condition witness element too short: expected {element_len} bytes, got {}",
                condition_data.len().saturating_sub(start)
            )));
        }

        elements.push(condition_data[start..end].to_vec());
        cursor.set_position(end as u64);
    }

    Ok(elements)
}

/// Finalize all virtual transactions needed to commit a VTXO on-chain.
pub fn finalize_unilateral_exit_tree(
    unilateral_exit_tree: &UnilateralExitTree,
    commitment_txs: &[Transaction],
) -> Result<Vec<Vec<Transaction>>, Error> {
    let mut finalized_virtual_tx_branches = Vec::new();
    for unilateral_exit_branch in unilateral_exit_tree.inner.iter() {
        let mut finalized_unilateral_exit_branch = Vec::new();
        for virtual_tx in unilateral_exit_branch.iter() {
            let psbt = virtual_tx.clone();

            let virtual_tx_previous_output =
                psbt.unsigned_tx.input[VTXO_INPUT_INDEX].previous_output;

            let witness_utxo = {
                unilateral_exit_branch
                    .iter()
                    .map(|p| &p.unsigned_tx)
                    .chain(commitment_txs.iter())
                    .find_map(|other_psbt| {
                        (other_psbt.compute_txid() == virtual_tx_previous_output.txid).then_some(
                            other_psbt.output[virtual_tx_previous_output.vout as usize].clone(),
                        )
                    })
            }
            .expect("witness UTXO in path");

            let tx = finalize_virtual_tx_input(psbt, VTXO_INPUT_INDEX, witness_utxo)?;

            finalized_unilateral_exit_branch.push(tx);
        }
        finalized_virtual_tx_branches.push(finalized_unilateral_exit_branch);
    }

    Ok(finalized_virtual_tx_branches)
}

#[deprecated(note = "use finalize_unilateral_exit_tree")]
pub fn sign_unilateral_exit_tree(
    unilateral_exit_tree: &UnilateralExitTree,
    commitment_txs: &[Transaction],
) -> Result<Vec<Vec<Transaction>>, Error> {
    finalize_unilateral_exit_tree(unilateral_exit_tree, commitment_txs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedUtxo {
    pub outpoint: OutPoint,
    pub amount: Amount,
    pub address: Address,
}

#[derive(Debug, Clone)]
pub struct UtxoCoinSelection {
    pub selected_utxos: Vec<SelectedUtxo>,
    pub total_selected: Amount,
    pub change_amount: Amount,
}

/// Build an anchor transaction by spending a 0-value P2A output and adding another output to cover
/// the transaction fees.
pub fn build_anchor_tx<F>(
    bumpable_tx: &Transaction,
    change_address: Address,
    fee_rate: f64,
    select_coins_fn: F,
) -> Result<Psbt, Error>
where
    F: FnOnce(Amount) -> Result<UtxoCoinSelection, Error>,
{
    let anchor = find_anchor_outpoint(bumpable_tx)?;

    // Estimate for the size of the bump transaction.
    const P2TR_KEYSPEND_INPUT_WEIGHT: u64 = 57 * 4 + 64; // 292 weight units
    const NESTED_P2WSH_INPUT_WEIGHT: u64 = 91 * 4 + 3 * 4; // 376 weight units
    const P2TR_OUTPUT_WEIGHT: u64 = 43 * 4; // 172 weight units

    // We assume only one UTXO will be selected to have a correct estimate.
    let estimated_weight = Weight::from_wu(
        NESTED_P2WSH_INPUT_WEIGHT + P2TR_KEYSPEND_INPUT_WEIGHT + P2TR_OUTPUT_WEIGHT,
    );

    let child_vsize = estimated_weight.to_vbytes_ceil();
    let package_size = child_vsize + bumpable_tx.weight().to_vbytes_ceil();

    let fee = Amount::from_sat((package_size as f64 * fee_rate).ceil() as u64);

    // Use dependency to select coins to cover the fee.
    let UtxoCoinSelection {
        selected_utxos,
        total_selected,
        change_amount,
    } = select_coins_fn(fee)?;

    if total_selected < fee {
        return Err(Error::coin_select(format!(
            "insufficient coins selected to cover {fee} fee"
        )));
    }

    // Build inputs and outputs.
    let mut inputs = vec![anchor];
    let mut sequences = vec![Sequence::MAX];

    for utxo in selected_utxos.iter() {
        inputs.push(utxo.outpoint);
        sequences.push(Sequence::MAX);
    }

    let outputs = vec![TxOut {
        value: change_amount,
        script_pubkey: change_address.script_pubkey(),
    }];

    // Create PSBT.
    let mut psbt = Psbt::from_unsigned_tx(Transaction {
        version: transaction::Version::non_standard(3),
        lock_time: LockTime::ZERO,
        input: inputs
            .iter()
            .zip(sequences.iter())
            .map(|(outpoint, sequence)| TxIn {
                previous_output: *outpoint,
                script_sig: ScriptBuf::new(),
                sequence: *sequence,
                witness: Witness::new(),
            })
            .collect(),
        output: outputs,
    })
    .map_err(|e| Error::transaction(format!("Failed to create PSBT: {e}")))?;

    // Set witness UTXO for anchor input (first input). The anchor input does not need signing,
    // hence the empty witness.
    psbt.inputs[0].witness_utxo = Some(anchor_output());
    psbt.inputs[0].final_script_witness = Some(Witness::new());

    // Set witness UTXO for the additional inputs (probably just one).
    for i in 1..psbt.inputs.len() {
        if let Some(utxo) = selected_utxos.get(i - 1) {
            psbt.inputs[i].witness_utxo = Some(TxOut {
                value: utxo.amount,
                script_pubkey: utxo.address.script_pubkey(),
            });
        }
    }

    Ok(psbt)
}

fn find_anchor_outpoint(tx: &Transaction) -> Result<OutPoint, Error> {
    let anchor_output_template = anchor_output();

    for (index, output) in tx.output.iter().enumerate() {
        if output == &anchor_output_template {
            return Ok(OutPoint {
                txid: tx.compute_txid(),
                vout: index as u32,
            });
        }
    }

    Err(Error::transaction("anchor output not found in transaction"))
}
