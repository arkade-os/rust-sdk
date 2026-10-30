use crate::script::extract_checksig_pubkeys;
use crate::Error;
use crate::ErrorContext;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::OP_CHECKSIG;
use bitcoin::opcodes::all::OP_CHECKSIGADD;
use bitcoin::opcodes::all::OP_CHECKSIGVERIFY;
use bitcoin::opcodes::all::OP_CODESEPARATOR;
use bitcoin::script::Instruction;
use bitcoin::secp256k1::Message;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot::LeafVersion;
use bitcoin::Psbt;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::XOnlyPublicKey;

/// Verify a server-signed Ark transaction before releasing checkpoint signatures.
///
/// The transaction must match `submitted`, and every CHECKSIG/CHECKSIGVERIFY key in
/// each submitted spend leaf must have a valid signature in `signed`. Only signatures
/// committing to all inputs and outputs are accepted. Prevouts and spend leaves are
/// taken exclusively from `submitted`, never from the server's response. This also
/// supports contracts using historical server keys without consulting current /info.
///
/// Supports one selected tapleaf per input, with literal public keys preceding each
/// CHECKSIG/CHECKSIGVERIFY, as used by the SDK's sends and VHTLCs. CHECKSIGADD and
/// CODESEPARATOR are unsupported. This verifies signatures, not script execution:
/// callers remain responsible for other conditions such as preimages and timelocks.
pub fn verify_signed_ark_transaction(submitted: &Psbt, signed: &Psbt) -> Result<(), Error> {
    if submitted.unsigned_tx != signed.unsigned_tx {
        return Err(Error::transaction(
            "server returned a different Ark transaction",
        ));
    }
    let count = submitted.unsigned_tx.input.len();
    if count == 0 || submitted.inputs.len() != count || signed.inputs.len() != count {
        return Err(Error::transaction("invalid Ark transaction input count"));
    }
    let prevouts = submitted
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            input.witness_utxo.as_ref().ok_or_else(|| {
                Error::transaction(format!("missing submitted prevout for input {i}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let secp = Secp256k1::verification_only();
    let mut sighashes = SighashCache::new(&submitted.unsigned_tx);

    for (i, input) in submitted.inputs.iter().enumerate() {
        if input.tap_scripts.len() != 1 {
            return Err(Error::transaction(format!(
                "expected one submitted spend leaf for input {i}"
            )));
        }
        let (control, (script, version)) = input
            .tap_scripts
            .first_key_value()
            .ok_or_else(|| Error::transaction("missing submitted spend leaf"))?;
        if *version != LeafVersion::TapScript || control.leaf_version != *version {
            return Err(Error::transaction(format!(
                "unsupported tapleaf version for input {i}"
            )));
        }
        let prevout_script = &prevouts[i].script_pubkey;
        if !prevout_script.is_p2tr() {
            return Err(Error::transaction(format!(
                "non-Taproot prevout for input {i}"
            )));
        }
        let output_key =
            XOnlyPublicKey::from_slice(&prevout_script.as_bytes()[2..]).map_err(Error::crypto)?;
        if !control.verify_taproot_commitment(&secp, output_key, script) {
            return Err(Error::transaction(format!(
                "spend leaf does not match prevout for input {i}"
            )));
        }

        let instructions = script
            .instructions()
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::transaction)?;
        if instructions.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::Op(OP_CODESEPARATOR | OP_CHECKSIGADD)
            )
        }) {
            return Err(Error::transaction(format!(
                "unsupported signature opcode for input {i}"
            )));
        }
        let checksig_count = instructions
            .iter()
            .filter(|instruction| {
                matches!(
                    instruction,
                    Instruction::Op(OP_CHECKSIG | OP_CHECKSIGVERIFY)
                )
            })
            .count();
        let keys = extract_checksig_pubkeys(script);
        if keys.is_empty() || keys.len() != checksig_count {
            return Err(Error::transaction(format!(
                "expected literal CHECKSIG public keys for input {i}"
            )));
        }
        let leaf_hash = TapLeafHash::from_script(script, *version);
        for key in keys {
            let signature = signed.inputs[i]
                .tap_script_sigs
                .get(&(key, leaf_hash))
                .ok_or_else(|| {
                    Error::transaction(format!("missing signature for input {i}, key {key}"))
                })?;
            if !matches!(
                signature.sighash_type,
                TapSighashType::Default | TapSighashType::All
            ) {
                return Err(Error::transaction(format!(
                    "signature must commit to all inputs and outputs for input {i}"
                )));
            }
            let hash = sighashes
                .taproot_script_spend_signature_hash(
                    i,
                    &Prevouts::All(&prevouts),
                    leaf_hash,
                    signature.sighash_type,
                )
                .map_err(Error::crypto)?;
            secp.verify_schnorr(
                &signature.signature,
                &Message::from_digest(hash.to_byte_array()),
                &key,
            )
            .map_err(Error::crypto)
            .with_context(|| format!("invalid signature for input {i}, key {key}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
