use bitcoin::opcodes::all::*;
use bitcoin::script::Instruction;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::fmt;

/// A conventional 2-of-2 multisignature [`ScriptBuf`].
pub fn multisig_script(pk_0: XOnlyPublicKey, pk_1: XOnlyPublicKey) -> ScriptBuf {
    ScriptBuf::builder()
        .push_x_only_key(&pk_0)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&pk_1)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

/// A [`ScriptBuf`] allowing the owner of `pk` to spend after `locktime_seconds` have passed from
/// the time the corresponding output was included in a block.
// TODO: Should support multisig.
pub fn csv_sig_script(locktime: bitcoin::Sequence, pk: XOnlyPublicKey) -> ScriptBuf {
    ScriptBuf::builder()
        .push_int(locktime.to_consensus_u32() as i64)
        .push_opcode(OP_CSV)
        .push_opcode(OP_DROP)
        .push_x_only_key(&pk)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

/// The script pubkey for the Taproot output corresponding to the given [`TaprootSpendInfo`].
pub fn tr_script_pubkey(spend_info: &TaprootSpendInfo) -> ScriptBuf {
    let output_key = spend_info.output_key();
    let builder = bitcoin::blockdata::script::Builder::new();
    builder
        .push_opcode(OP_PUSHNUM_1)
        .push_slice(output_key.serialize())
        .into_script()
}

/// Extracts all [`XOnlyPublicKey`]s from checksig patterns in the script.
///
/// Finds all 32-byte data pushes that are immediately followed by
/// [`OP_CHECKSIG`] or [`OP_CHECKSIGVERIFY`] opcodes.
///
/// Returns an empty vector if no matching keys are found.
pub fn extract_checksig_pubkeys(script: &ScriptBuf) -> Vec<XOnlyPublicKey> {
    let instructions: Vec<_> = script.instructions().filter_map(|inst| inst.ok()).collect();

    let mut pubkeys = Vec::new();

    for window in instructions.windows(2) {
        let (push, checksig) = (&window[0], &window[1]);

        // Check if we have a 32-byte push followed by CHECKSIG or CHECKSIGVERIFY
        if let Instruction::PushBytes(bytes) = push {
            if bytes.len() != 32 {
                continue;
            }

            let is_checksig = matches!(
                checksig,
                Instruction::Op(op) if *op == OP_CHECKSIG || *op == OP_CHECKSIGVERIFY
            );

            if let Ok(pk) = XOnlyPublicKey::from_slice(bytes.as_bytes()) {
                if is_checksig {
                    pubkeys.push(pk);
                }
            }
        }
    }

    pubkeys
}

pub fn extract_sequence_from_csv_sig_script(
    script: &ScriptBuf,
) -> Result<bitcoin::Sequence, InvalidCsvSigScriptError> {
    let csv_index = script
        .to_bytes()
        .windows(2)
        .position(|window| *window == [OP_CSV.to_u8(), OP_DROP.to_u8()])
        .ok_or(InvalidCsvSigScriptError)?;

    let before_csv = &script.to_bytes()[..csv_index];

    // It is either `OP_PUSHNUM_X` (a single byte) or `OP_PUSH_BYTES_X BYTES` (more than one
    // byte).
    let sequence = if before_csv.len() > 1 {
        &before_csv[1..]
    } else {
        before_csv
    };

    let mut sequence = sequence.to_vec();
    sequence.reverse();

    let mut buffer = [0u8; 4];
    let input_len = sequence.len();
    let start_index = 4 - input_len; // calculate how many spaces to leave at the front

    buffer[start_index..].copy_from_slice(&sequence);

    let sequence = u32::from_be_bytes(buffer);

    let sequence = bitcoin::Sequence::from_consensus(sequence);

    Ok(sequence)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCsvSigScriptError;

impl fmt::Display for InvalidCsvSigScriptError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("invalid CSV-Sig script")
    }
}

impl std::error::Error for InvalidCsvSigScriptError {}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::locktime;
    use bitcoin::XOnlyPublicKey;
    use std::str::FromStr;

    #[test]
    fn test_extract_sequence_from_csv_sig_script() {
        // Equivalent to two 512-second intervals.
        let locktime_seconds = 1024;
        let sequence = bitcoin::Sequence::from_seconds_ceil(locktime_seconds).unwrap();

        let pk = XOnlyPublicKey::from_str(
            "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();

        let script = csv_sig_script(sequence, pk);

        let parsed = extract_sequence_from_csv_sig_script(&script).unwrap();
        let parsed = parsed.to_relative_lock_time();

        assert_eq!(
            parsed,
            locktime::relative::LockTime::from_512_second_intervals(2).into()
        );
    }

    #[test]
    fn test_extract_checksig_pubkeys_from_multisig() {
        let pk_0 = XOnlyPublicKey::from_str(
            "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let pk_1 = XOnlyPublicKey::from_str(
            "28845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();

        let script = multisig_script(pk_0, pk_1);
        let pubkeys = extract_checksig_pubkeys(&script);

        assert_eq!(pubkeys.len(), 2);
        assert_eq!(pubkeys[0], pk_0);
        assert_eq!(pubkeys[1], pk_1);
    }

    #[test]
    fn test_extract_checksig_pubkeys_from_csv_sig() {
        let pk = XOnlyPublicKey::from_str(
            "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let sequence = bitcoin::Sequence::from_seconds_ceil(1024).unwrap();

        let script = csv_sig_script(sequence, pk);
        let pubkeys = extract_checksig_pubkeys(&script);

        assert_eq!(pubkeys.len(), 1);
        assert_eq!(pubkeys[0], pk);
    }

    #[test]
    fn test_extract_checksig_pubkeys_empty_script() {
        let script = ScriptBuf::new();
        let pubkeys = extract_checksig_pubkeys(&script);

        assert!(pubkeys.is_empty());
    }

    #[test]
    fn test_extract_checksig_pubkeys_no_checksig() {
        // Script with only OP_DROP and OP_RETURN, no checksig
        let script = ScriptBuf::builder()
            .push_opcode(OP_DROP)
            .push_opcode(OP_RETURN)
            .into_script();

        let pubkeys = extract_checksig_pubkeys(&script);

        assert!(pubkeys.is_empty());
    }
}
