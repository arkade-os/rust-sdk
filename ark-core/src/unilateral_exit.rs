use crate::server::Round;
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
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::Witness;
use std::collections::HashMap;
use std::collections::HashSet;

/// A UTXO that could have become a VTXO with the help of the Ark server, but is now unilaterally
/// spendable by the original owner.
#[derive(Debug, Clone)]
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

pub struct VtxoProvenance {
    /// Where the VTXO would end up on the blockchain if it were to become a UTXO.
    outpoint: OutPoint,
    /// The ID of the round transaction from which this VTXO comes from.
    round_txid: Txid,
    /// If this is an unconfirmed (out-of-round) VTXO, this is the redeem transaction the VTXO is
    /// an output of.
    // TODO: Given that an unconfirmed VTXO can come from another unconfirmed VTXO, one transaction
    // is not enough! Thus, this should be a list.
    redeem_transaction: Option<Psbt>,
}

impl VtxoProvenance {
    pub fn new(outpoint: OutPoint, round_txid: Txid) -> Self {
        Self {
            outpoint,
            round_txid,
            redeem_transaction: None,
        }
    }

    pub fn new_unconfirmed(outpoint: OutPoint, round_txid: Txid, redeem_transaction: Psbt) -> Self {
        Self {
            outpoint,
            round_txid,
            redeem_transaction: Some(redeem_transaction),
        }
    }

    pub fn round_txid(&self) -> Txid {
        self.round_txid
    }
}

/// Generate a list of transactions that must be confirmed on the blockchain as a prerequisite to
/// spending the given `vtxo_inputs`.
///
/// For all the `vtxo_inputs` provided, the caller must ensure that the `rounds` argument contains a
/// matching [`Round`]. Failure to do so will result in an error.
///
/// ### Explanation
///
/// For all the `vtxo_inputs` that the caller wants to unilaterally convert into UTXOs, we must
/// first ensure that all the ancestor transactions (AKA the redeem branch) in the VTXO tree have
/// been confirmed on the blockchain.
///
/// For example, given the following basic tree
///
/// [Round TX: <VTXO tree Output>]
///                     |
///                  [TX A: <Internal node>]
///                                 |
///                              [TX B: <VTXO X>]
///
/// if `VTXO X` is included in `vtxo_inputs`, the function will return `[TX A, TX B]`.
///
/// ### Returns
///
/// A list of transactions that must be confirmed before all the `vtxo_inputs` can be spent.
///
/// The order of the transactions ensures that a transaction will never appear before an unpublished
/// parent transaction. This guarantees that the caller can publish the transactions in the given
/// order.
///
/// There are no repeats in the transaction list.
///
/// Some of the transactions may have already been published. Thus, the caller may need to skip
/// publishing certain transactions that have already been published.
pub fn prepare_vtxo_tree_transactions(
    vtxos: &[VtxoProvenance],
    rounds: HashMap<Txid, Round>,
) -> Result<Vec<Transaction>, Error> {
    let mut vtxo_trees = HashMap::new();
    let mut redeem_branches = HashMap::new();
    for VtxoProvenance {
        outpoint,
        round_txid,
        redeem_transaction,
    } in vtxos.iter()
    {
        let round = rounds
            .get(round_txid)
            .ok_or_else(|| Error::ad_hoc(format!("missing info for round {round_txid}")))?;

        // TODO: If this VTXO is an output of a redeem transaction, we should walk back up the chain
        // of redeem transactions and the VTXO tree to unilaterally off-board this VTXO.
        if redeem_transaction.is_some() {
            tracing::warn!(
                %outpoint,
                "Spending unconfirmed VTXOs unilaterally is not supported yet. Skipping"
            );

            continue;
        }

        let round_psbt = &round.round_tx;

        let round_tx = round
            .round_tx
            .clone()
            .extract_tx()
            .map_err(Error::transaction)?;
        let round_txid = round_tx.compute_txid();
        vtxo_trees
            .entry(round_txid)
            .or_insert_with(|| round.vtxo_tree.clone());

        let vtxo_tree = vtxo_trees.get(&round_txid).expect("is there");

        let root = &vtxo_tree.levels[0].nodes[0];

        let vtxo_txid = outpoint.txid;
        let leaf_node = vtxo_tree
            .levels
            .last()
            .expect("at least one")
            .nodes
            .iter()
            .find(|node| node.txid == vtxo_txid)
            .expect("leaf node");

        // Build the branch from our VTXO to the root of the VTXO tree.
        let mut branch = vec![leaf_node];
        while branch[0].txid != root.txid {
            let parent_node = vtxo_tree
                .levels
                .iter()
                .find_map(|level| level.nodes.iter().find(|node| node.txid == branch[0].txid))
                .expect("parent");

            branch = [vec![parent_node], branch].concat()
        }

        let branch = branch
            .into_iter()
            .map(|node| node.tx.clone())
            .collect::<Vec<_>>();

        redeem_branches.insert(vtxo_txid, (RedeemBranch { branch }, round_psbt.clone()));
    }

    let mut tx_set = HashSet::new();
    let mut all_txs = Vec::new();
    for (redeem_branch, round_psbt) in redeem_branches.values() {
        for psbt in redeem_branch.branch.iter() {
            let mut psbt = psbt.clone();

            let vtxo_previous_output = psbt.unsigned_tx.input[VTXO_INPUT_INDEX].previous_output;

            let witness_utxo = {
                redeem_branch
                    .branch
                    .iter()
                    .chain(std::iter::once(round_psbt))
                    .find_map(|other_psbt| {
                        (other_psbt.unsigned_tx.compute_txid() == vtxo_previous_output.txid)
                            .then_some(
                                other_psbt.unsigned_tx.output[vtxo_previous_output.vout as usize]
                                    .clone(),
                            )
                    })
            }
            .expect("witness utxo in path");

            psbt.inputs[VTXO_INPUT_INDEX].witness_utxo = Some(witness_utxo);

            let tap_key_sig =
                psbt.inputs[VTXO_INPUT_INDEX]
                    .tap_key_sig
                    .ok_or(Error::transaction(
                        "missing taproot key spend signature in VTXO transaction",
                    ))?;

            psbt.inputs[VTXO_INPUT_INDEX].final_script_witness =
                Some(Witness::p2tr_key_spend(&tap_key_sig));

            let tx = psbt.clone().extract_tx().map_err(Error::transaction)?;

            let txid = tx.compute_txid();
            if !tx_set.contains(&txid) {
                tx_set.insert(txid);
                all_txs.push(tx);
            }
        }
    }

    Ok(all_txs)
}

struct RedeemBranch {
    branch: Vec<Psbt>,
}
