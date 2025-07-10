use crate::ArkNote;
use crate::Error;
use crate::ErrorContext;
use bitcoin::absolute::LockTime;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::*;
use bitcoin::psbt::PsbtSighashType;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::transaction::Version;
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
use bitcoin::Witness;
use bitcoin::XOnlyPublicKey;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug)]
pub struct Input {
    // The TXID of this outpoint is a hash of the TXID of the actual outpoint.
    outpoint: OutPoint,
    sequence: Sequence,
    witness_utxo: TxOut,
    // We do not serialize this.
    tapscripts: Vec<ScriptBuf>,
    pk: XOnlyPublicKey,
    spend_info: (ScriptBuf, taproot::ControlBlock),
    is_onchain: bool,
}

impl From<&ArkNote> for Input {
    fn from(value: &ArkNote) -> Self {
        let taprootkey = value.vtxo_script().spend_info().output_key();
        let script_pubkey = value.to_tx_out().script_pubkey;

        let spending_info = value.vtxo_script().spend_info();

        // FIXME: remove the unwrap!
        let spending_scripts = value.vtxo_script().tap_leaves();
        let spending_scripts = spending_scripts.first().unwrap();
        let spending_script = spending_scripts.0.clone();
        let spending_control_block = spending_scripts.1.clone();

        Self {
            outpoint: value.outpoint(),
            sequence: Sequence::MAX,
            witness_utxo: TxOut {
                value: value.value(),
                script_pubkey: script_pubkey.clone(),
            },
            // FIXME: I completely made it up
            tapscripts: vec![],
            // FIXME: I completely made it up
            pk: taprootkey.to_x_only_public_key(),
            spend_info: (spending_script, spending_control_block),
            // FIXME: I completely made it up
            is_onchain: false,
        }
    }
}

impl Input {
    pub fn new(
        outpoint: OutPoint,
        sequence: Sequence,
        witness_utxo: TxOut,
        tapscripts: Vec<ScriptBuf>,
        pk: XOnlyPublicKey,
        spend_info: (ScriptBuf, taproot::ControlBlock),
        is_onchain: bool,
    ) -> Self {
        Self {
            outpoint,
            sequence,
            witness_utxo,
            tapscripts,
            pk,
            spend_info,
            is_onchain,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Output {
    /// An output created when boarding.
    Offchain(TxOut),
    /// An output created when offboarding.
    Onchain(TxOut),
}

pub struct Bip322Proof(Transaction);

impl Bip322Proof {
    pub fn serialize(&self) -> String {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = bitcoin::consensus::encode::serialize(&self.0);
        base64.encode(&bytes)
    }
}

pub fn make_bip322_signature<F>(
    // TODO: In theory, should be a `Vec`.
    signing_kp: &Keypair,
    sign_for_onchain_pk_fn: F,
    inputs: Vec<Input>,
    outputs: Vec<Output>,
    own_cosigner_pks: Vec<PublicKey>,
) -> Result<(Bip322Proof, IntentMessage), Error>
where
    F: Fn(&XOnlyPublicKey, &secp256k1::Message) -> Result<schnorr::Signature, Error>,
{
    let mut input_tap_trees = Vec::new();
    for input in inputs.iter() {
        let input_taptree = input
            .tapscripts
            .iter()
            .map(|t| t.to_hex_string())
            .collect::<Vec<_>>();

        let input_taptree = taptree::TapTree(input_taptree)
            .encode()
            .map_err(Error::ad_hoc)
            .context("failed to encode input taptree")?;

        input_tap_trees.push(input_taptree.to_lower_hex_string());
    }

    let mut onchain_output_indexes = Vec::new();
    for (i, output) in outputs.iter().enumerate() {
        if matches!(output, Output::Onchain(_)) {
            onchain_output_indexes.push(i);
        }
    }

    let now = SystemTime::now();
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_err(Error::ad_hoc)
        .context("failed to compute now timestamp")?;
    let now = now.as_secs();
    let expire_at = now + (2 * 60);

    let intent_message = IntentMessage {
        intent_message_type: IntentMessageType::Register,
        input_tap_trees,
        onchain_output_indexes,
        valid_at: now,
        expire_at,
        own_cosigner_pks,
    };

    let (mut proof_psbt, fake_input) = build_proof_psbt(&intent_message, &inputs, &outputs)?;

    for (i, proof_input) in proof_psbt.inputs.iter_mut().enumerate() {
        let leaf_proof = if i == 0 {
            inputs[0].spend_info.clone()
        } else {
            inputs[i - 1].spend_info.clone()
        };

        proof_input.tap_scripts = BTreeMap::from_iter([(
            leaf_proof.1.clone(),
            (leaf_proof.0.clone(), taproot::LeafVersion::TapScript),
        )]);
    }

    let secp = Secp256k1::new();

    let prevouts = proof_psbt
        .inputs
        .iter()
        .filter_map(|i| i.witness_utxo.clone())
        .collect::<Vec<_>>();

    let inputs = [inputs, vec![fake_input]].concat();

    for (i, proof_input) in proof_psbt.inputs.iter_mut().enumerate() {
        let input = inputs
            .iter()
            .find(|input| input.outpoint == proof_psbt.unsigned_tx.input[i].previous_output)
            .expect("witness utxo");

        let prevouts = Prevouts::All(&prevouts);

        let (exit_control_block, (exit_script, leaf_version)) =
            proof_input.tap_scripts.first_key_value().expect("a value");

        let leaf_hash = TapLeafHash::from_script(exit_script, *leaf_version);

        let tap_sighash = SighashCache::new(&proof_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(i, &prevouts, leaf_hash, TapSighashType::Default)
            .map_err(Error::crypto)
            .with_context(|| format!("failed to compute sighash for proof of funds input {i}"))?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let pk = input.pk;

        let sig = match input.is_onchain {
            true => {
                let sig = sign_for_onchain_pk_fn(&pk, &msg)?;

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify own proof of funds boarding output signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                proof_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);

                sig
            }
            false => {
                let sig = secp.sign_schnorr_no_aux_rand(&msg, signing_kp);

                secp.verify_schnorr(&sig, &msg, &pk)
                    .map_err(Error::crypto)
                    .context("failed to verify own proof of funds vtxo signature")?;

                let sig = taproot::Signature {
                    signature: sig,
                    sighash_type: TapSighashType::Default,
                };

                proof_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);

                sig
            }
        };

        let witness = Witness::from_slice(&[
            &sig.signature[..],
            exit_script.as_bytes(),
            &exit_control_block.serialize(),
        ]);

        proof_input.final_script_witness = Some(witness);
    }

    let signed_proof_tx = proof_psbt
        .extract_tx()
        .map_err(Error::crypto)
        .context("failed to extract signed proof TX")?;

    Ok((Bip322Proof(signed_proof_tx), intent_message))
}

fn build_proof_psbt(
    message: &IntentMessage,
    inputs: &[Input],
    outputs: &[Output],
) -> Result<(Psbt, Input), Error> {
    if inputs.is_empty() {
        return Err(Error::ad_hoc("missing inputs"));
    }

    let message = message
        .encode()
        .map_err(Error::ad_hoc)
        .context("failed to encode intent message")?;

    let first_input = inputs[0].clone();
    let script_pubkey = first_input.witness_utxo.script_pubkey.clone();

    let to_spend_tx = {
        let hash = bip322_hash(message.as_bytes());

        let script_sig = ScriptBuf::builder()
            .push_opcode(OP_PUSHBYTES_0)
            .push_slice(hash.as_byte_array())
            .into_script();

        let output = TxOut {
            value: Amount::ZERO,
            script_pubkey,
        };

        Transaction {
            version: Version::non_standard(0),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::all_zeros(),
                    vout: 0xFFFFFFFF,
                },
                script_sig,
                sequence: Sequence::ZERO,
                witness: Witness::default(),
            }],
            output: vec![output.clone()],
        }
    };

    let fake_outpoint = OutPoint {
        txid: to_spend_tx.compute_txid(),
        vout: 0,
    };

    let to_sign_psbt = {
        let mut to_sign_inputs = Vec::with_capacity(inputs.len() + 1);

        to_sign_inputs.push(TxIn {
            previous_output: fake_outpoint,
            script_sig: ScriptBuf::new(),
            sequence: first_input.sequence,
            witness: Witness::default(),
        });

        for input in inputs.iter() {
            to_sign_inputs.push(TxIn {
                previous_output: input.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: input.sequence,
                witness: Witness::default(),
            });
        }

        let outputs = match outputs.len() {
            0 => vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new_op_return([]),
            }],
            _ => outputs
                .iter()
                .map(|o| match o {
                    Output::Offchain(txout) | Output::Onchain(txout) => txout.clone(),
                })
                .collect::<Vec<_>>(),
        };

        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: to_sign_inputs,
            output: outputs,
        };

        let mut psbt = Psbt::from_unsigned_tx(tx)
            .map_err(Error::ad_hoc)
            .context("failed to build proof of funds PSBT")?;

        psbt.inputs[0].witness_utxo = Some(to_spend_tx.output[0].clone());
        psbt.inputs[0].sighash_type = Some(PsbtSighashType::from_u32(1));

        for (i, input) in inputs.iter().enumerate() {
            psbt.inputs[i + 1].witness_utxo = Some(input.witness_utxo.clone());
            psbt.inputs[i + 1].sighash_type = Some(TapSighashType::Default.into());
        }

        psbt
    };

    let mut first_input_modified = first_input.clone();
    first_input_modified.outpoint = fake_outpoint;

    Ok((to_sign_psbt, first_input_modified))
}

fn bip322_hash(message: &[u8]) -> sha256::Hash {
    const TAG: &[u8] = b"BIP0322-signed-message";

    let hashed_tag = sha256::Hash::hash(TAG);

    let mut v = Vec::new();
    v.extend_from_slice(hashed_tag.as_byte_array());
    v.extend_from_slice(hashed_tag.as_byte_array());
    v.extend_from_slice(message);

    sha256::Hash::hash(&v)
}

#[derive(Serialize)]
pub struct IntentMessage {
    #[serde(rename = "type")]
    intent_message_type: IntentMessageType,
    input_tap_trees: Vec<String>,
    // Indicates which outputs are on-chain out of all the outputs we are registering.
    onchain_output_indexes: Vec<usize>,
    // The time when this intent message is valid from.
    valid_at: u64,
    // The time when this intent message is no longer valid.
    expire_at: u64,
    #[serde(rename = "cosigners_public_keys")]
    own_cosigner_pks: Vec<PublicKey>,
}

impl IntentMessage {
    pub fn encode(&self) -> Result<String, Error> {
        // TODO: Probably should get rid of `serde` and `serde_json` if we serialize manually.
        serde_json::to_string(self)
            .map_err(Error::ad_hoc)
            .context("failed to serialize intent message to JSON")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IntentMessageType {
    Register,
    Delete,
}

mod taptree {
    use bitcoin::hex::FromHex;
    use std::io::Write;
    use std::io::{self};

    pub struct TapTree(pub Vec<String>);

    impl TapTree {
        pub fn encode(&self) -> io::Result<Vec<u8>> {
            let mut tapscripts_bytes = Vec::new();

            // write number of leaves as compact size uint
            write_compact_size_uint(&mut tapscripts_bytes, self.0.len() as u64)?;

            for tapscript in &self.0 {
                let script_bytes = Vec::from_hex(tapscript)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "hex decode error"))?;

                // write depth (always 1)
                tapscripts_bytes.push(1);

                // write leaf version (base leaf version: 0xc0)
                tapscripts_bytes.push(0xc0);

                // write script
                write_compact_size_uint(&mut tapscripts_bytes, script_bytes.len() as u64)?;
                tapscripts_bytes.extend(&script_bytes);
            }

            Ok(tapscripts_bytes)
        }

        #[cfg(test)]
        pub fn decode(data: &[u8]) -> io::Result<Self> {
            use bitcoin::hex::DisplayHex;
            use std::io::Cursor;
            use std::io::Read;

            let mut buf = Cursor::new(data);
            let count = read_compact_size_uint(&mut buf)?;

            let mut leaves = Vec::with_capacity(count as usize);

            for _ in 0..count {
                // depth : ignore
                let mut depth = [0u8; 1];
                buf.read_exact(&mut depth)?;

                // leaf version : ignore, we assume base tapscript
                let mut lv = [0u8; 1];
                buf.read_exact(&mut lv)?;

                // script length
                let script_len = read_compact_size_uint(&mut buf)? as usize;

                // script bytes
                let mut script_bytes = vec![0u8; script_len];
                buf.read_exact(&mut script_bytes)?;

                leaves.push(script_bytes.to_lower_hex_string());
            }

            Ok(TapTree(leaves))
        }
    }

    // Write compact size uint to writer
    fn write_compact_size_uint<W: Write>(w: &mut W, val: u64) -> io::Result<()> {
        if val < 253 {
            w.write_all(&[val as u8])
        } else if val < 0x10000 {
            w.write_all(&[253])?;
            w.write_all(&(val as u16).to_le_bytes())
        } else if val < 0x100000000 {
            w.write_all(&[254])?;
            w.write_all(&(val as u32).to_le_bytes())
        } else {
            w.write_all(&[255])?;
            w.write_all(&val.to_le_bytes())
        }
    }

    #[cfg(test)]
    // Read compact size uint from reader
    fn read_compact_size_uint<R: io::Read>(r: &mut R) -> io::Result<u64> {
        let mut first = [0u8; 1];
        r.read_exact(&mut first)?;
        match first[0] {
            253 => {
                let mut buf = [0u8; 2];
                r.read_exact(&mut buf)?;
                Ok(u16::from_le_bytes(buf) as u64)
            }
            254 => {
                let mut buf = [0u8; 4];
                r.read_exact(&mut buf)?;
                Ok(u32::from_le_bytes(buf) as u64)
            }
            255 => {
                let mut buf = [0u8; 8];
                r.read_exact(&mut buf)?;
                Ok(u64::from_le_bytes(buf))
            }
            v => Ok(v as u64),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn tap_tree_encode_decode_roundtrip() {
            // sample tapscript (OP_TRUE)
            let tapscript_hex = "51";
            let tree = TapTree(vec![tapscript_hex.into()]);
            let encoded = tree.encode().unwrap();
            let decoded = TapTree::decode(&encoded).unwrap();
            assert_eq!(decoded.0, vec![tapscript_hex]);
        }

        #[test]
        fn tap_tree_multiple_leaves() {
            let tapscript_hex1 = "51";
            let tapscript_hex2 = "52";
            let tree = TapTree(vec![tapscript_hex1.into(), tapscript_hex2.into()]);
            let encoded = tree.encode().unwrap();
            let decoded = TapTree::decode(&encoded).unwrap();
            assert_eq!(decoded.0, vec![tapscript_hex1, tapscript_hex2]);
        }
    }
}
