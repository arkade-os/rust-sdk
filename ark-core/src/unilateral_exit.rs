use crate::redeem::anchor_output;
use crate::server;
use crate::BoardingOutput;
use crate::Error;
use crate::ErrorContext;
use crate::Vtxo;
use crate::VTXO_INPUT_INDEX;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::transaction;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::Sequence;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::Weight;
use bitcoin::Witness;
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

    pub fn previous_output(&self) -> TxOut {
        TxOut {
            value: self.amount,
            script_pubkey: self.vtxo.script_pubkey(),
        }
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
pub fn create_unilateral_exit_transaction(
    kp: &Keypair,
    to_address: Address,
    to_amount: Amount,
    change_address: Address,
    onchain_inputs: &[OnChainInput],
    vtxo_inputs: &[VtxoInput],
) -> Result<Transaction, Error> {
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
        .chain(vtxo_inputs.iter().map(|v| v.amount))
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
            sequence: v.vtxo.exit_delay(),
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

        let txout = onchain_inputs
            .iter()
            .find_map(|o| {
                (o.outpoint == outpoint).then_some(TxOut {
                    value: o.amount,
                    script_pubkey: o.boarding_output.address().script_pubkey(),
                })
            })
            .or_else(|| {
                vtxo_inputs.iter().find_map(|v| {
                    (v.outpoint == outpoint).then_some(TxOut {
                        value: v.amount,
                        script_pubkey: v.vtxo.address().script_pubkey(),
                    })
                })
            })
            .expect("txout for input");

        input.witness_utxo = Some(txout);
    }

    // Collect all `witness_utxo` entries.
    let prevouts = psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    // Sign each input.
    for (i, input) in psbt.inputs.iter_mut().enumerate() {
        let outpoint = psbt.unsigned_tx.input[i].previous_output;

        let (exit_script, exit_control_block) = onchain_inputs
            .iter()
            .find_map(|b| (b.outpoint == outpoint).then(|| b.boarding_output.exit_spend_info()))
            .or_else(|| {
                vtxo_inputs
                    .iter()
                    .find_map(|v| (v.outpoint == outpoint).then(|| v.vtxo.exit_spend_info()))
            })
            .expect("spend info for input");

        let leaf_version = exit_control_block.leaf_version;
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

        let sig = secp.sign_schnorr_no_aux_rand(&msg, kp);
        let pk = kp.x_only_public_key().0;

        secp.verify_schnorr(&sig, &msg, &pk)
            .map_err(Error::crypto)
            .with_context(|| format!("failed to verify own signature for input {i}"))?;

        let witness = Witness::from_slice(&[
            &sig[..],
            exit_script.as_bytes(),
            &exit_control_block.serialize(),
        ]);

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

/// Build the unilateral exit tree of TXIDs for a VTXO from a [`sever::VtxoChains`].
pub fn build_unilateral_exit_tree_txids(
    vtxo_chains: &server::VtxoChains,
    // The TXID of the VTXO we want to commit on-chain.
    virtual_txid: Txid,
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
            return Err(Error::ad_hoc("chain traversal lead to cycle"));
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

        for chained_tx in &chain.spends {
            match chained_tx.tx_type {
                server::ChainedTxType::Commitment => {
                    // We've reached our destination.
                    all_paths.push(current_path.clone());

                    reached_commitment = true;
                }
                server::ChainedTxType::Virtual => {
                    // Continue traversing virtual transactions up the tree.
                    find_paths_to_commitment(
                        chained_tx.txid,
                        chain_map,
                        current_path,
                        all_paths,
                        visited,
                    )?;
                }
                server::ChainedTxType::Unspecified => {
                    tracing::warn!(
                        txid = %chained_tx.txid,
                        "Found unspecified TX type when walking up virtual TX tree. \
                         Treating it like a virtual TX"
                    );

                    // Continue traversing virtual transactions up the tree.
                    find_paths_to_commitment(
                        chained_tx.txid,
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
        virtual_txid,
        &chain_map,
        &mut current_path,
        &mut all_paths,
        &mut visited,
    )?;

    if all_paths.is_empty() {
        return Err(Error::ad_hoc(format!(
            "no paths found from virtual TX {virtual_txid} to commitment transaction",
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

/// The full path from commitment transaction to VTXO. The entire path will need to be published
/// on-chain to execute a unilateral exit with this VTXO.
///
/// We use the word "tree" because a VTXO may come from more than one path i.e. if its corresponding
/// virtual transaction has more than one input!
pub struct UnilateralExitTree {
    /// The commitment transaction from which this VTXO comes from.
    round_txid: Txid,
    /// The chains of virtual transactions that lead to a VTXO.
    ///
    /// Virtual TXs in a branch are ordered by distance to the root commitment transaction, with
    /// virtual TXs closest to it appearing first.
    inner: Vec<Vec<Psbt>>,
}

impl UnilateralExitTree {
    pub fn new(round_txid: Txid, virtual_tx_branches: Vec<Vec<Psbt>>) -> Self {
        Self {
            round_txid,
            inner: virtual_tx_branches,
        }
    }

    pub fn round_txid(&self) -> Txid {
        self.round_txid
    }

    pub fn inner(&self) -> &Vec<Vec<Psbt>> {
        &self.inner
    }
}

/// Sign all the transactions needed to commit a VTXO on-chain.
pub fn sign_unilateral_exit_tree(
    unilateral_exit_tree: &UnilateralExitTree,
    round_tx: &Transaction,
) -> Result<Vec<Vec<Transaction>>, Error> {
    let mut signed_virtual_tx_branches = Vec::new();
    for unilateral_exit_branch in unilateral_exit_tree.inner.iter() {
        let mut signed_unilateral_exit_branch = Vec::new();
        for virtual_tx in unilateral_exit_branch.iter() {
            let txid = virtual_tx.unsigned_tx.compute_txid();
            let mut psbt = virtual_tx.clone();

            let vtxo_previous_output = psbt.unsigned_tx.input[VTXO_INPUT_INDEX].previous_output;

            let witness_utxo = {
                unilateral_exit_branch
                    .iter()
                    .map(|p| &p.unsigned_tx)
                    .chain(std::iter::once(round_tx))
                    .find_map(|other_psbt| {
                        (other_psbt.compute_txid() == vtxo_previous_output.txid).then_some(
                            other_psbt.output[vtxo_previous_output.vout as usize].clone(),
                        )
                    })
            }
            .expect("witness UTXO in path");

            psbt.inputs[VTXO_INPUT_INDEX].witness_utxo = Some(witness_utxo);

            if let Some(tap_key_sig) = psbt.inputs[VTXO_INPUT_INDEX].tap_key_sig {
                tracing::debug!(%txid, "Signing key spend for confirmed VTXO");

                psbt.inputs[VTXO_INPUT_INDEX].final_script_witness =
                    Some(Witness::p2tr_key_spend(&tap_key_sig));
            } else if !psbt.inputs[VTXO_INPUT_INDEX].tap_script_sigs.is_empty() {
                tracing::debug!(%txid, "Signing script spend for pre-confirmed VTXO");

                // We assume that there is only one script. TODO: May need to revise this.
                let tap_scripts = psbt.inputs[VTXO_INPUT_INDEX].tap_scripts.iter().next();
                let tap_script_sigs = psbt.inputs[VTXO_INPUT_INDEX].tap_script_sigs.values();

                let (control_block, (script, _)) = tap_scripts.ok_or_else(|| {
                    Error::transaction(format!("missing tapscripts in virtual TX {txid}"))
                })?;

                // Construct witness: [sig1, sig2, script, control_block].
                let mut witness = Witness::new();

                // We assume that the signatures are in the correct order. TODO: May need to
                // revise this.
                for sig in tap_script_sigs {
                    witness.push(sig.to_vec());
                }

                witness.push(script.as_bytes());
                witness.push(control_block.serialize());

                psbt.inputs[VTXO_INPUT_INDEX].final_script_witness = Some(witness);
            } else {
                return Err(Error::transaction(format!(
                    "missing taproot key spend or script spend data in virtual TX {txid}"
                )));
            };

            let tx = psbt.clone().extract_tx().map_err(Error::transaction)?;

            signed_unilateral_exit_branch.push(tx);
        }
        signed_virtual_tx_branches.push(signed_unilateral_exit_branch);
    }

    Ok(signed_virtual_tx_branches)
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
                script_sig: bitcoin::ScriptBuf::new(),
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
