use crate::tx_weight_estimator;
use crate::tx_weight_estimator::compute_redeem_tx_fee;
use crate::vtxo::Vtxo;
use crate::ArkAddress;
use crate::Error;
use crate::ErrorContext;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::transaction;
use bitcoin::Amount;
use bitcoin::FeeRate;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::XOnlyPublicKey;
use std::collections::BTreeMap;

/// A VTXO to be spent into an unconfirmed VTXO.
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
}

/// Build a transaction to send VTXOs to another [`ArkAddress`].
pub fn build_redeem_transaction(
    outputs: &[(&ArkAddress, Amount)],
    change_address: Option<&ArkAddress>,
    vtxo_inputs: &[VtxoInput],
) -> Result<Psbt, Error> {
    if vtxo_inputs.is_empty() {
        return Err(Error::transaction(
            "cannot build redeem transaction without inputs",
        ));
    }

    let mut outputs = outputs
        .iter()
        .map(|(address, amount)| TxOut {
            value: *amount,
            script_pubkey: address.to_p2tr_script_pubkey(),
        })
        .collect::<Vec<_>>();

    let total_input_amount: Amount = vtxo_inputs.iter().map(|v| v.amount).sum();
    let total_output_amount: Amount = outputs.iter().map(|v| v.value).sum();

    let change_amount = total_input_amount.checked_sub(total_output_amount).ok_or_else(|| {
        Error::transaction(format!(
            "cannot cover total output amount ({total_output_amount}) with total input amount ({total_input_amount})"
        ))
    })?;

    let (change_index, extra_fee) = if change_amount > Amount::ZERO {
        match change_address {
            Some(change_address) => {
                outputs.push(TxOut {
                    value: change_amount,
                    script_pubkey: change_address.to_p2tr_script_pubkey(),
                });

                (Some(outputs.len() - 1), Amount::ZERO)
            }
            None => (None, change_amount),
        }
    } else {
        (None, Amount::ZERO)
    };

    let fee = {
        let vtxos = vtxo_inputs
            .iter()
            .map(
                |VtxoInput {
                     vtxo,
                     amount,
                     outpoint,
                 }| {
                    let (script, control_block) = vtxo.forfeit_spend_info();

                    tx_weight_estimator::VtxoInput {
                        outpoint: *outpoint,
                        amount: *amount,
                        revealed_script: Some(script),
                        control_block,
                        witness_size: Vtxo::FORFEIT_WITNESS_SIZE,
                    }
                },
            )
            .collect::<Vec<_>>();

        let computed_fee = compute_redeem_tx_fee(
            FeeRate::from_sat_per_kwu(253),
            vtxos.as_slice(),
            outputs.len(),
        )
        .map_err(Error::from)?;

        computed_fee + extra_fee
    };

    // Subtract the fee from somewhere.
    //
    // TODO: We need a more robust solution.
    match change_index {
        Some(change_index) => {
            let change_output = outputs.get_mut(change_index).expect("change output");

            let change_amount = change_output.value;
            change_output.value = change_amount.checked_sub(fee).ok_or_else(|| {
                Error::coin_select("fee ({fee}) greater than change ({change_amount})")
            })?;
        }
        // If there is no change output, subtract fee evenly from all outputs.
        _ => {
            let fee_per_output = fee / (outputs.len() as u64);

            for output in outputs.iter_mut() {
                output.value = output.value.checked_sub(fee_per_output).ok_or_else(|| {
                    Error::transaction(format!(
                        "failed to subtract fee portion ({fee_per_output}) from output ({})",
                        output.value
                    ))
                })?;
            }
        }
    };

    // TODO: Use a different locktime if we have CLTV multisig script.
    let lock_time = LockTime::ZERO;

    let unsigned_tx = Transaction {
        version: transaction::Version::TWO,
        lock_time,
        input: vtxo_inputs
            .iter()
            .map(|VtxoInput { outpoint, .. }| TxIn {
                previous_output: *outpoint,
                script_sig: Default::default(),
                // TODO: Use a different sequence number if we have a CLTV multisig script.
                sequence: bitcoin::Sequence::MAX,
                witness: Default::default(),
            })
            .collect(),
        output: outputs,
    };

    let unsigned_redeem_psbt = Psbt::from_unsigned_tx(unsigned_tx).map_err(Error::transaction)?;

    Ok(unsigned_redeem_psbt)
}

/// Sign an input for the given redeem transaction.
pub fn sign_redeem_transaction<S>(
    sign_fn: S,
    redeem_psbt: &mut Psbt,
    vtxo_inputs: &[VtxoInput],
    input_index: usize,
) -> Result<(), Error>
where
    S: FnOnce(secp256k1::Message) -> Result<(schnorr::Signature, XOnlyPublicKey), Error>,
{
    let VtxoInput {
        vtxo,
        amount,
        outpoint,
    } = vtxo_inputs
        .get(input_index)
        .ok_or_else(|| Error::ad_hoc(format!("no input to sign at index {input_index}")))?;

    tracing::debug!(
        ?outpoint,
        %amount,
        ?vtxo,
        "Attempting to sign selected VTXO for redeem transaction"
    );

    let prevout = TxOut {
        value: *amount,
        script_pubkey: vtxo.script_pubkey(),
    };

    let (input_index, _) = redeem_psbt
        .unsigned_tx
        .input
        .iter()
        .enumerate()
        .find(|(_, input)| input.previous_output == *outpoint)
        .ok_or_else(|| Error::transaction(format!("missing input for outpoint {outpoint}")))?;

    tracing::debug!(
        ?outpoint,
        ?vtxo,
        index = input_index,
        "Signing selected VTXO for redeem transaction"
    );

    let psbt_input = redeem_psbt
        .inputs
        .get_mut(input_index)
        .expect("input at index");

    psbt_input.witness_utxo = Some(prevout.clone());

    // In the case of input VTXOs, we are actually using a script spend path.
    let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info();

    let leaf_version = forfeit_control_block.leaf_version;
    psbt_input.tap_scripts = BTreeMap::from_iter([(
        forfeit_control_block,
        (forfeit_script.clone(), leaf_version),
    )]);

    let prevouts = vtxo_inputs
        .iter()
        .map(|v| TxOut {
            value: v.amount,
            script_pubkey: v.vtxo.script_pubkey(),
        })
        .collect::<Vec<_>>();
    let prevouts = Prevouts::All(&prevouts);

    let leaf_hash = TapLeafHash::from_script(&forfeit_script, leaf_version);

    let tap_sighash = SighashCache::new(&redeem_psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(
            input_index,
            &prevouts,
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(Error::crypto)
        .context("failed to generate sighash")?;

    let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

    let (sig, pk) = sign_fn(msg)?;

    let sig = taproot::Signature {
        signature: sig,
        sighash_type: TapSighashType::Default,
    };

    psbt_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);

    Ok(())
}
