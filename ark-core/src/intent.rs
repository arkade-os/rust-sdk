use crate::Error;
use crate::ErrorContext;
use crate::VTXO_TAPROOT_KEY;
use bitcoin::absolute;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::*;
use bitcoin::psbt;
use bitcoin::psbt::PsbtSighashType;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
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
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug)]
pub struct Input {
    // The TXID of this outpoint is a hash of the TXID of the actual outpoint.
    outpoint: OutPoint,
    // Related to OP_CSV (such as unilateral exit for all VTXOs).
    sequence: Sequence,
    // Related to OP_CLTV (such as the timelock in a HTLC).
    locktime: absolute::LockTime,
    witness_utxo: TxOut,
    // We do not serialize this.
    tapscripts: Vec<ScriptBuf>,
    spend_info: (ScriptBuf, taproot::ControlBlock),
    is_onchain: bool,
    is_swept: bool,
}

impl Input {
    pub fn new(
        outpoint: OutPoint,
        sequence: Sequence,
        locktime: Option<absolute::LockTime>,
        witness_utxo: TxOut,
        tapscripts: Vec<ScriptBuf>,
        spend_info: (ScriptBuf, taproot::ControlBlock),
        is_onchain: bool,
        is_swept: bool,
    ) -> Self {
        Self {
            outpoint,
            sequence,
            locktime: locktime.unwrap_or(absolute::LockTime::ZERO),
            witness_utxo,
            tapscripts,
            spend_info,
            is_onchain,
            is_swept,
        }
    }

    pub fn script_pubkey(&self) -> &ScriptBuf {
        &self.witness_utxo.script_pubkey
    }

    pub fn amount(&self) -> Amount {
        self.witness_utxo.value
    }

    pub fn spend_info(&self) -> &(ScriptBuf, taproot::ControlBlock) {
        &self.spend_info
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }

    pub fn tapscripts(&self) -> &[ScriptBuf] {
        &self.tapscripts
    }

    pub fn is_swept(&self) -> bool {
        self.is_swept
    }
}

#[derive(Debug, Clone)]
pub enum Output {
    /// An output created when boarding.
    Offchain(TxOut),
    /// An output created when offboarding.
    Onchain(TxOut),
}

#[derive(Debug, Clone)]
pub struct Intent {
    pub proof: Psbt,
    message: IntentMessage,
}

impl Intent {
    pub fn new(proof: Psbt, message: IntentMessage) -> Self {
        Self { proof, message }
    }

    pub fn serialize_proof(&self) -> String {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = self.proof.serialize();

        base64.encode(&bytes)
    }

    pub fn serialize_message(&self) -> Result<String, Error> {
        self.message.encode()
    }
}

pub fn make_intent<SV, SO>(
    sign_for_vtxo_fn: SV,
    sign_for_onchain_fn: SO,
    inputs: Vec<Input>,
    outputs: Vec<Output>,
    own_cosigner_pks: Vec<PublicKey>,
) -> Result<Intent, Error>
where
    SV: Fn(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, Error>,
    SO: Fn(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<(schnorr::Signature, XOnlyPublicKey), Error>,
{
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
        onchain_output_indexes,
        valid_at: now,
        expire_at,
        own_cosigner_pks,
    };

    let (mut proof_psbt, fake_input) = build_proof_psbt(&intent_message, &inputs, &outputs)?;

    for (i, proof_input) in proof_psbt.inputs.iter_mut().enumerate() {
        if i == 0 {
            let (script, control_block) = inputs[0].spend_info.clone();

            proof_input
                .tap_scripts
                .insert(control_block, (script, taproot::LeafVersion::TapScript));
        } else {
            let (script, control_block) = inputs[i - 1].spend_info.clone();

            let tap_tree = taptree::TapTree(inputs[i - 1].tapscripts.clone());
            let bytes = tap_tree
                .encode()
                .map_err(Error::ad_hoc)
                .with_context(|| format!("failed to encode taptree for input {i}"))?;

            proof_input.unknown.insert(
                psbt::raw::Key {
                    type_value: 222,
                    key: VTXO_TAPROOT_KEY.to_vec(),
                },
                bytes,
            );
            proof_input
                .tap_scripts
                .insert(control_block, (script, taproot::LeafVersion::TapScript));
        };
    }

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

        let (_, (script, leaf_version)) =
            proof_input.tap_scripts.first_key_value().expect("a value");

        let leaf_hash = TapLeafHash::from_script(script, *leaf_version);

        let tap_sighash = SighashCache::new(&proof_psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(i, &prevouts, leaf_hash, TapSighashType::Default)
            .map_err(Error::crypto)
            .with_context(|| format!("failed to compute sighash for proof of funds input {i}"))?;

        let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

        let sigs = match input.is_onchain {
            true => vec![sign_for_onchain_fn(proof_input, msg)?],
            false => sign_for_vtxo_fn(proof_input, msg)?,
        };

        for (sig, pk) in sigs {
            let sig = taproot::Signature {
                signature: sig,
                sighash_type: TapSighashType::Default,
            };
            proof_input.tap_script_sigs.insert((pk, leaf_hash), sig);
        }
    }

    Ok(Intent {
        proof: proof_psbt,
        message: intent_message,
    })
}

pub(crate) fn build_proof_psbt(
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
        let hash = message_hash(message.as_bytes());

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
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::all_zeros(),
                    vout: 0xFFFFFFFF,
                },
                script_sig,
                sequence: Sequence::ZERO,
                witness: Witness::default(),
            }],
            output: vec![output],
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
            lock_time: inputs
                .iter()
                .map(|i| i.locktime)
                .max_by(|a, b| a.to_consensus_u32().cmp(&b.to_consensus_u32()))
                .unwrap_or(absolute::LockTime::ZERO),
            input: to_sign_inputs,
            output: outputs,
        };

        let mut psbt = Psbt::from_unsigned_tx(tx)
            .map_err(Error::ad_hoc)
            .context("failed to build proof of funds PSBT")?;

        psbt.inputs[0].witness_utxo = Some(to_spend_tx.output[0].clone());
        psbt.inputs[0].sighash_type = Some(PsbtSighashType::from_u32(1));
        psbt.inputs[0].witness_script = Some(inputs[0].spend_info.0.clone());

        for (i, input) in inputs.iter().enumerate() {
            psbt.inputs[i + 1].witness_utxo = Some(input.witness_utxo.clone());
            psbt.inputs[i + 1].sighash_type = Some(PsbtSighashType::from_u32(1));
            psbt.inputs[i + 1].witness_script = Some(input.spend_info.0.clone());
        }

        psbt
    };

    let mut first_input_modified = first_input;
    first_input_modified.outpoint = fake_outpoint;

    Ok((to_sign_psbt, first_input_modified))
}

fn message_hash(message: &[u8]) -> sha256::Hash {
    const TAG: &[u8] = b"ark-intent-proof-message";

    let hashed_tag = sha256::Hash::hash(TAG);

    let mut v = Vec::new();
    v.extend_from_slice(hashed_tag.as_byte_array());
    v.extend_from_slice(hashed_tag.as_byte_array());
    v.extend_from_slice(message);

    sha256::Hash::hash(&v)
}

#[derive(Serialize, Debug, Clone)]
pub struct IntentMessage {
    #[serde(rename = "type")]
    intent_message_type: IntentMessageType,
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
    pub(crate) fn new(
        intent_message_type: IntentMessageType,
        onchain_output_indexes: Vec<usize>,
        valid_at: u64,
        expire_at: u64,
        own_cosigner_pks: Vec<PublicKey>,
    ) -> Self {
        Self {
            intent_message_type,
            onchain_output_indexes,
            valid_at,
            expire_at,
            own_cosigner_pks,
        }
    }

    pub fn encode(&self) -> Result<String, Error> {
        // TODO: Probably should get rid of `serde` and `serde_json` if we serialize manually.
        serde_json::to_string(self)
            .map_err(Error::ad_hoc)
            .context("failed to serialize intent message to JSON")
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum IntentMessageType {
    Register,
    Delete,
}

pub(crate) mod taptree {
    use bitcoin::ScriptBuf;
    use std::io::Write;
    use std::io::{self};

    pub struct TapTree(pub Vec<ScriptBuf>);

    impl TapTree {
        pub fn encode(&self) -> io::Result<Vec<u8>> {
            let mut tapscripts_bytes = Vec::new();
            for tapscript in &self.0 {
                // write depth (always 1)
                tapscripts_bytes.push(1);

                // write leaf version (base leaf version: 0xc0)
                tapscripts_bytes.push(0xc0);

                // write script
                write_compact_size_uint(&mut tapscripts_bytes, tapscript.len() as u64)?;
                tapscripts_bytes.extend(tapscript.as_bytes());
            }

            Ok(tapscripts_bytes)
        }

        #[cfg(test)]
        pub fn decode(data: &[u8]) -> io::Result<Self> {
            use std::io::Cursor;
            use std::io::Read;

            let mut buf = Cursor::new(data);
            let mut leaves = Vec::new();

            // Read leaves until we run out of data
            while buf.position() < data.len() as u64 {
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

                leaves.push(ScriptBuf::from_bytes(script_bytes));
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
        use bitcoin::opcodes::OP_FALSE;
        use bitcoin::opcodes::OP_TRUE;

        #[test]
        fn tap_tree_encode_decode_roundtrip() {
            let scripts = vec![ScriptBuf::builder().push_opcode(OP_TRUE).into_script()];

            let tree = TapTree(scripts.clone());
            let encoded = tree.encode().unwrap();
            let decoded = TapTree::decode(&encoded).unwrap();
            assert_eq!(decoded.0, scripts);
        }

        #[test]
        fn tap_tree_multiple_leaves() {
            let scripts = vec![
                ScriptBuf::builder().push_opcode(OP_TRUE).into_script(),
                ScriptBuf::builder().push_opcode(OP_FALSE).into_script(),
            ];
            let tree = TapTree(scripts.clone());
            let encoded = tree.encode().unwrap();
            let decoded = TapTree::decode(&encoded).unwrap();
            assert_eq!(decoded.0, scripts);
        }
    }
}
