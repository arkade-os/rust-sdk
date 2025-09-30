use crate::anchor_output;
use crate::script::csv_sig_script;
use crate::script::tr_script_pubkey;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use crate::ErrorContext;
use crate::UNSPENDABLE_KEY;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::PublicKey;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::sighash::Prevouts;
use bitcoin::sighash::SighashCache;
use bitcoin::taproot;
use bitcoin::taproot::ControlBlock;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::transaction;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::XOnlyPublicKey;
use std::collections::BTreeMap;
use std::io;
use std::io::Write;

/// The byte value corresponds to the string "taptree".
const VTXO_TAPROOT_KEY: [u8; 7] = [116, 97, 112, 116, 114, 101, 101];

/// The byte value corresponds to the string "condition".
pub const VTXO_CONDITION_KEY: [u8; 9] = [99, 111, 110, 100, 105, 116, 105, 111, 110];

/// The byte value corresponds to the string "expiry".
pub const VTXO_TREE_EXPIRY_PSBT_KEY: [u8; 6] = [101, 120, 112, 105, 114, 121];

/// A VTXO to be spent into an unconfirmed VTXO.
#[derive(Debug, Clone)]
pub struct VtxoInput {
    /// The script path that will be used to spend the [`Vtxo`].
    ///
    /// The very same spend path is also used when building the corresponding checkpoint output.
    spend_script: ScriptBuf,
    /// An optional locktime, only set if the `spend_script` uses `OP_CLTV`.
    // TODO: Parse this information from the script instead.
    locktime: Option<LockTime>,
    control_block: ControlBlock,
    /// All the scripts in the Taproot tree.
    tapscripts: Vec<ScriptBuf>,
    script_pubkey: ScriptBuf,
    /// The amount of coins locked in the VTXO.
    amount: Amount,
    /// Where the VTXO would end up on the blockchain if it were to become a UTXO.
    outpoint: OutPoint,
}

impl VtxoInput {
    pub fn new(
        vtxo_spend_script: ScriptBuf,
        locktime: Option<LockTime>,
        control_block: ControlBlock,
        tapscripts: Vec<ScriptBuf>,
        script_pubkey: ScriptBuf,
        amount: Amount,
        outpoint: OutPoint,
    ) -> Self {
        Self {
            spend_script: vtxo_spend_script,
            locktime,
            control_block,
            tapscripts,
            script_pubkey,
            amount,
            outpoint,
        }
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }

    pub fn spend_info(&self) -> (&ScriptBuf, &ControlBlock) {
        (&self.spend_script, &self.control_block)
    }
}

#[derive(Debug, Clone)]
pub struct OffchainTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<(Psbt, CheckpointOutput, CheckpointOutPoint, VtxoInput)>,
}

/// Build a transaction to send VTXOs to another [`ArkAddress`].
pub fn build_offchain_transactions(
    outputs: &[(&ArkAddress, Amount)],
    change_address: Option<&ArkAddress>,
    vtxo_inputs: &[VtxoInput],
    server_info: &server::Info,
) -> Result<OffchainTransactions, Error> {
    if vtxo_inputs.is_empty() {
        return Err(Error::transaction(
            "cannot build Ark transaction without inputs",
        ));
    }

    let checkpoint_exit_script = csv_sig_script(
        server_info.unilateral_exit_delay,
        server_info.pk.x_only_public_key().0,
    );

    let mut checkpoint_txs = Vec::new();
    for vtxo_input in vtxo_inputs.iter() {
        let (psbt, checkpoint_output, checkpoint_out_point) =
            build_checkpoint_psbt(vtxo_input, checkpoint_exit_script.clone()).with_context(
                || {
                    format!(
                        "failed to build checkpoint psbt for input {:?}",
                        vtxo_input.outpoint
                    )
                },
            )?;

        checkpoint_txs.push((
            psbt,
            checkpoint_output,
            checkpoint_out_point,
            vtxo_input.clone(),
        ));
    }

    let mut outputs = outputs
        .iter()
        .map(|(address, amount)| {
            if *amount > server_info.dust {
                TxOut {
                    value: *amount,
                    script_pubkey: address.to_p2tr_script_pubkey(),
                }
            } else {
                TxOut {
                    value: *amount,
                    script_pubkey: address.to_sub_dust_script_pubkey(),
                }
            }
        })
        .collect::<Vec<_>>();

    let total_input_amount: Amount = vtxo_inputs.iter().map(|v| v.amount).sum();
    let total_output_amount: Amount = outputs.iter().map(|v| v.value).sum();

    let change_amount = total_input_amount.checked_sub(total_output_amount).ok_or_else(|| {
        Error::transaction(format!(
            "cannot cover total output amount ({total_output_amount}) with total input amount ({total_input_amount})"
        ))
    })?;

    if change_amount > Amount::ZERO {
        if let Some(change_address) = change_address {
            if change_amount > server_info.dust {
                outputs.push(TxOut {
                    value: change_amount,
                    script_pubkey: change_address.to_p2tr_script_pubkey(),
                })
            } else {
                outputs.push(TxOut {
                    value: change_amount,
                    script_pubkey: change_address.to_sub_dust_script_pubkey(),
                })
            }
        }
    }

    outputs.push(anchor_output());

    let timelocked_inputs = vtxo_inputs
        .iter()
        .filter_map(|x| x.locktime)
        .collect::<Vec<_>>();

    let highest_timelock = timelocked_inputs
        .iter()
        .try_fold(None, |acc, a| match (acc, a) {
            (None, locktime) => Ok(Some(*locktime)),
            (Some(a @ LockTime::Blocks(h1)), LockTime::Blocks(h2)) if h1 > *h2 => Ok(Some(a)),
            (Some(LockTime::Blocks(_)), b @ LockTime::Blocks(_)) => Ok(Some(*b)),
            (Some(a @ LockTime::Seconds(t1)), LockTime::Seconds(t2)) if t1 > *t2 => Ok(Some(a)),
            (Some(LockTime::Seconds(_)), b @ LockTime::Seconds(_)) => Ok(Some(*b)),
            _ => Err(Error::transaction("incompatible locktimes")),
        })?;

    let (lock_time, sequence) = match highest_timelock {
        Some(timelock) => (timelock, bitcoin::Sequence::ENABLE_LOCKTIME_NO_RBF),
        None => (LockTime::ZERO, bitcoin::Sequence::MAX),
    };

    let unsigned_ark_tx = Transaction {
        version: transaction::Version::non_standard(3),
        lock_time,
        input: checkpoint_txs
            .iter()
            .map(|(_, _, CheckpointOutPoint { outpoint, .. }, _)| TxIn {
                previous_output: *outpoint,
                script_sig: Default::default(),
                sequence,
                witness: Default::default(),
            })
            .collect(),
        output: outputs,
    };

    let mut unsigned_ark_psbt =
        Psbt::from_unsigned_tx(unsigned_ark_tx).map_err(Error::transaction)?;

    for (i, (_, checkpoint_output, _, _)) in checkpoint_txs.iter().enumerate() {
        let mut bytes = Vec::new();

        let script = &checkpoint_output.vtxo_spend_script;
        write_compact_size_uint(&mut bytes, script.len() as u64).map_err(Error::transaction)?;

        // Write the depth (always 1). TODO: Support more depth.
        bytes.push(1);

        // TODO: Support future leaf versions.
        bytes.push(LeafVersion::TapScript.to_consensus());

        let mut script_bytes = script.to_bytes();

        write_compact_size_uint(&mut bytes, script_bytes.len() as u64)
            .map_err(Error::transaction)?;

        bytes.append(&mut script_bytes);

        unsigned_ark_psbt.inputs[i].unknown.insert(
            psbt::raw::Key {
                type_value: u8::MAX,
                key: VTXO_TAPROOT_KEY.to_vec(),
            },
            bytes,
        );
    }

    Ok(OffchainTransactions {
        ark_tx: unsigned_ark_psbt,
        checkpoint_txs,
    })
}

#[derive(Debug, Clone)]
pub struct CheckpointOutput {
    vtxo_spend_script: ScriptBuf,
    spend_info: TaprootSpendInfo,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckpointOutPoint {
    outpoint: OutPoint,
    amount: Amount,
}

impl CheckpointOutput {
    fn new(vtxo_input: &VtxoInput, checkpoint_exit_script: ScriptBuf) -> Self {
        let secp = Secp256k1::new();

        let unspendable_key: PublicKey = UNSPENDABLE_KEY.parse().expect("valid key");
        let (unspendable_key, _) = unspendable_key.inner.x_only_public_key();

        let vtxo_spend_script = &vtxo_input.spend_script;

        let spend_info = TaprootBuilder::new()
            .add_leaf(1, vtxo_spend_script.clone())
            .expect("valid spend leaf")
            .add_leaf(1, checkpoint_exit_script)
            .expect("valid exit leaf")
            .finalize(&secp, unspendable_key)
            .expect("can be finalized");

        Self {
            vtxo_spend_script: vtxo_spend_script.clone(),
            spend_info,
        }
    }

    fn script_pubkey(&self) -> ScriptBuf {
        tr_script_pubkey(&self.spend_info)
    }
}

fn build_checkpoint_psbt(
    vtxo_input: &VtxoInput,
    // An alternative way for the _server_ unilaterally spend the checkpoint output, in case the
    // owner does not spend it.
    //
    // This must be a "CSV Multisig" script, with only a single PK: the server PK.
    checkpoint_exit_script: ScriptBuf,
) -> Result<(Psbt, CheckpointOutput, CheckpointOutPoint), Error> {
    let (lock_time, sequence) = match vtxo_input.locktime {
        Some(timelock) => (timelock, bitcoin::Sequence::ENABLE_LOCKTIME_NO_RBF),
        None => (LockTime::ZERO, bitcoin::Sequence::MAX),
    };

    let inputs = vec![TxIn {
        previous_output: vtxo_input.outpoint,
        script_sig: Default::default(),
        sequence,
        witness: Default::default(),
    }];

    let checkpoint_output = CheckpointOutput::new(vtxo_input, checkpoint_exit_script);

    let outputs = vec![
        TxOut {
            value: vtxo_input.amount,
            script_pubkey: checkpoint_output.script_pubkey(),
        },
        anchor_output(),
    ];

    let unsigned_tx = Transaction {
        version: transaction::Version::non_standard(3),
        lock_time,
        input: inputs,
        output: outputs,
    };

    let mut unsigned_checkpoint_psbt =
        Psbt::from_unsigned_tx(unsigned_tx).map_err(Error::transaction)?;

    let mut bytes = Vec::new();

    write_compact_size_uint(&mut bytes, vtxo_input.tapscripts.len() as u64)
        .map_err(Error::transaction)?;

    for script in vtxo_input.tapscripts.iter() {
        // Write the depth (always 1). TODO: Support more depth.
        bytes.push(1);

        // TODO: Support future leaf versions.
        bytes.push(LeafVersion::TapScript.to_consensus());

        let mut script_bytes = script.to_bytes();

        write_compact_size_uint(&mut bytes, script_bytes.len() as u64)
            .map_err(Error::transaction)?;

        bytes.append(&mut script_bytes);
    }

    unsigned_checkpoint_psbt.inputs[0].witness_utxo = Some(TxOut {
        value: vtxo_input.amount,
        script_pubkey: vtxo_input.script_pubkey.clone(),
    });

    // In the case of input VTXOs, we are actually using a script spend path.
    let (vtxo_spend_script, vtxo_spend_control_block) = vtxo_input.spend_info();

    let leaf_version = vtxo_spend_control_block.leaf_version;
    unsigned_checkpoint_psbt.inputs[0].tap_scripts = BTreeMap::from_iter([(
        vtxo_spend_control_block.clone(),
        (vtxo_spend_script.clone(), leaf_version),
    )]);

    unsigned_checkpoint_psbt.inputs[0].unknown.insert(
        psbt::raw::Key {
            type_value: u8::MAX,
            key: VTXO_TAPROOT_KEY.to_vec(),
        },
        bytes,
    );

    let checkpoint_outpoint = CheckpointOutPoint {
        outpoint: OutPoint {
            txid: unsigned_checkpoint_psbt.unsigned_tx.compute_txid(),
            vout: 0,
        },
        amount: vtxo_input.amount,
    };

    Ok((
        unsigned_checkpoint_psbt,
        checkpoint_output,
        checkpoint_outpoint,
    ))
}

fn write_compact_size_uint<W: Write>(w: &mut W, val: u64) -> io::Result<()> {
    if val < 253 {
        w.write_all(&[val as u8])?;
    } else if val < 0x10000 {
        w.write_all(&[253])?;
        w.write_all(&(val as u16).to_le_bytes())?;
    } else if val < 0x100000000 {
        w.write_all(&[254])?;
        w.write_all(&(val as u32).to_le_bytes())?;
    } else {
        w.write_all(&[255])?;
        w.write_all(&val.to_le_bytes())?;
    }
    Ok(())
}

pub fn sign_checkpoint_transaction<S>(
    sign_fn: S,
    psbt: &mut Psbt,
    vtxo_input: &VtxoInput,
) -> Result<(), Error>
where
    S: FnOnce(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<(schnorr::Signature, XOnlyPublicKey), Error>,
{
    let VtxoInput {
        amount,
        outpoint,
        script_pubkey,
        ..
    } = vtxo_input;

    tracing::debug!(
        ?outpoint,
        %amount,
        "Attempting to sign selected VTXO for checkpoint transaction"
    );

    let (input_index, _) = psbt
        .unsigned_tx
        .input
        .iter()
        .enumerate()
        .find(|(_, input)| input.previous_output == *outpoint)
        .ok_or_else(|| Error::transaction(format!("missing input for outpoint {outpoint}")))?;

    tracing::debug!(
        ?outpoint,
        index = input_index,
        "Signing selected VTXO for checkpoint transaction"
    );

    let psbt_input = psbt.inputs.get_mut(input_index).expect("input at index");

    // In the case of input VTXOs, we are actually using a script spend path.
    let (vtxo_spend_script, vtxo_spend_control_block) = vtxo_input.spend_info();

    let leaf_version = vtxo_spend_control_block.leaf_version;

    let prevouts = [TxOut {
        value: *amount,
        script_pubkey: script_pubkey.clone(),
    }];
    let prevouts = Prevouts::All(&prevouts);

    let leaf_hash = TapLeafHash::from_script(vtxo_spend_script, leaf_version);

    let tap_sighash = SighashCache::new(&psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(
            input_index,
            &prevouts,
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(Error::crypto)
        .context("failed to generate sighash")?;

    let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

    let (sig, pk) = sign_fn(psbt_input, msg)?;

    let sig = taproot::Signature {
        signature: sig,
        sighash_type: TapSighashType::Default,
    };

    // FIXME(server): We were able to delete the server's signature here and it did not complain. We
    // were then unable to perform unilateral exit (same for the server I think).
    psbt_input.tap_script_sigs.insert((pk, leaf_hash), sig);

    Ok(())
}

pub fn sign_ark_transaction<S>(
    sign_fn: S,
    psbt: &mut Psbt,
    checkpoint_inputs: &[(CheckpointOutput, CheckpointOutPoint)],
    input_index: usize,
) -> Result<(), Error>
where
    S: FnOnce(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<(schnorr::Signature, XOnlyPublicKey), Error>,
{
    let (checkpoint_output, CheckpointOutPoint { outpoint, amount }) = checkpoint_inputs
        .get(input_index)
        .ok_or_else(|| Error::ad_hoc(format!("no input to sign at index {input_index}")))?;

    tracing::debug!(
        ?outpoint,
        %amount,
        "Attempting to sign selected checkpoint output for Ark transaction"
    );

    let prevout = TxOut {
        value: *amount,
        script_pubkey: checkpoint_output.script_pubkey(),
    };

    psbt.unsigned_tx
        .input
        .iter()
        .enumerate()
        .find(|(_, input)| input.previous_output == *outpoint)
        .ok_or_else(|| Error::transaction(format!("missing input for outpoint {outpoint}")))?;

    tracing::debug!(
        ?outpoint,
        index = input_index,
        "Signing checkpoint output for Ark transaction"
    );

    let psbt_input = psbt.inputs.get_mut(input_index).expect("input at index");

    psbt_input.witness_utxo = Some(prevout.clone());

    // In the case of input checkpoint outputs, we are using a script spend path.

    let vtxo_spend_script = &checkpoint_output.vtxo_spend_script;
    let leaf_version = LeafVersion::TapScript;

    let control_block = checkpoint_output
        .spend_info
        .control_block(&(vtxo_spend_script.clone(), leaf_version))
        .ok_or_else(|| {
            Error::transaction(format!(
                "failed to construct control block for input {outpoint:?}"
            ))
        })?;

    psbt_input.tap_scripts =
        BTreeMap::from_iter([(control_block, (vtxo_spend_script.clone(), leaf_version))]);

    let prevouts = checkpoint_inputs
        .iter()
        .map(|(output, outpoint)| TxOut {
            value: outpoint.amount,
            script_pubkey: output.script_pubkey(),
        })
        .collect::<Vec<_>>();
    let prevouts = Prevouts::All(&prevouts);

    let leaf_hash = TapLeafHash::from_script(vtxo_spend_script, leaf_version);

    let tap_sighash = SighashCache::new(&psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(
            input_index,
            &prevouts,
            leaf_hash,
            TapSighashType::Default,
        )
        .map_err(Error::crypto)
        .context("failed to generate sighash")?;

    let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

    let (sig, pk) = sign_fn(psbt_input, msg)?;

    let sig = taproot::Signature {
        signature: sig,
        sighash_type: TapSighashType::Default,
    };

    psbt_input.tap_script_sigs = BTreeMap::from_iter([((pk, leaf_hash), sig)]);

    Ok(())
}
