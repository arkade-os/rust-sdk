use crate::anchor_output;
use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::script::tr_script_pubkey;
use crate::server;
use crate::ArkAddress;
use crate::Asset;
use crate::Error;
use crate::ErrorContext;
use crate::UNSPENDABLE_KEY;
use crate::VTXO_TAPROOT_KEY;
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
use std::collections::HashMap;
use std::io;
use std::io::Write;

pub mod issue_asset;
pub mod reissue_asset;

pub use issue_asset::build_self_asset_issuance_transactions;
pub use issue_asset::SelfAssetIssuanceTransactions;
pub use reissue_asset::build_asset_reissuance_transactions;
pub use reissue_asset::AssetReissuanceTransactions;

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
    /// All the assets carried by this VTXO.
    assets: Vec<Asset>,
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
        assets: Vec<Asset>,
    ) -> Self {
        Self {
            spend_script: vtxo_spend_script,
            locktime,
            control_block,
            tapscripts,
            script_pubkey,
            amount,
            outpoint,
            assets,
        }
    }

    pub fn outpoint(&self) -> OutPoint {
        self.outpoint
    }

    pub fn spend_info(&self) -> (&ScriptBuf, &ControlBlock) {
        (&self.spend_script, &self.control_block)
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.script_pubkey.clone()
    }

    pub fn amount(&self) -> Amount {
        self.amount
    }

    pub fn assets(&self) -> &[Asset] {
        &self.assets
    }
}

/// A receiver for a generic offchain send with optional assets.
#[derive(Debug, Clone)]
pub struct SendReceiver {
    pub address: ArkAddress,
    pub amount: Amount,
    pub assets: Vec<Asset>,
}

impl SendReceiver {
    pub fn bitcoin(address: ArkAddress, amount: Amount) -> Self {
        Self {
            address,
            amount,
            assets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OffchainTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build a transaction to send VTXOs to another [`ArkAddress`].
pub(crate) fn btc_change_output_index(ark_tx: &Psbt, num_receiver_outputs: usize) -> Option<u16> {
    (ark_tx.unsigned_tx.output.len() > num_receiver_outputs + 1)
        .then_some((ark_tx.unsigned_tx.output.len() - 2) as u16)
}

/// Build unsigned offchain transactions for sending BTC to one or more receivers.
///
/// Receiver outputs are assigned in the same order as `receivers`, followed by an optional BTC
/// change output and the final anchor output.
///
/// # Arguments
///
/// * `receivers` - Offchain recipients and the BTC amounts assigned to each transaction output. Any
///   assets carried on [`SendReceiver`] values are ignored by this builder.
/// * `change_address` - The sender's offchain change address, used if the transaction has BTC
///   change
/// * `vtxo_inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `server_info` - Server configuration used to build the offchain transaction shape and dust
///   output
///
/// # Returns
///
/// [`OffchainTransactions`] containing the unsigned Ark transaction and unsigned checkpoint
/// transactions.
///
/// This function is intentionally packet-agnostic: it builds the BTC transaction skeleton only and
/// does not attach an asset packet. Callers that need asset semantics should either add exactly
/// one packet themselves or use [`build_asset_send_transactions`] for the generic asset-send flow.
///
/// # Errors
///
/// Returns an error if unsigned offchain transaction construction fails.
pub fn build_offchain_transactions(
    receivers: &[SendReceiver],
    change_address: &ArkAddress,
    vtxo_inputs: &[VtxoInput],
    server_info: &server::Info,
) -> Result<OffchainTransactions, Error> {
    if vtxo_inputs.is_empty() {
        return Err(Error::transaction(
            "cannot build Ark transaction without inputs",
        ));
    }

    let vtxo_min_amount = server_info.vtxo_min_amount.unwrap_or(Amount::ONE_SAT);
    if receivers
        .iter()
        .any(|SendReceiver { amount, .. }| *amount < vtxo_min_amount)
    {
        return Err(Error::transaction(format!(
            "output amount smaller than minimum of {vtxo_min_amount}"
        )));
    }

    let checkpoint_script = &server_info.checkpoint_tapscript;

    let mut checkpoint_data = Vec::new();
    for vtxo_input in vtxo_inputs.iter() {
        let (psbt, spend_info) = build_checkpoint_psbt(vtxo_input, checkpoint_script.clone())
            .with_context(|| {
                format!(
                    "failed to build checkpoint psbt for input {:?}",
                    vtxo_input.outpoint
                )
            })?;

        checkpoint_data.push((psbt, spend_info));
    }

    let mut outputs = receivers
        .iter()
        .map(
            |SendReceiver {
                 address, amount, ..
             }| {
                if *amount >= server_info.dust {
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
            },
        )
        .collect::<Vec<_>>();

    let total_input_amount: Amount = vtxo_inputs.iter().map(|v| v.amount).sum();
    let total_output_amount: Amount = outputs.iter().map(|v| v.value).sum();

    let change_amount = total_input_amount.checked_sub(total_output_amount).ok_or_else(|| {
        Error::transaction(format!(
            "cannot cover total output amount ({total_output_amount}) with total input amount ({total_input_amount})"
        ))
    })?;

    if change_amount > Amount::ZERO {
        if change_amount >= server_info.dust {
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
        input: checkpoint_data
            .iter()
            .map(|(psbt, _)| TxIn {
                previous_output: OutPoint {
                    txid: psbt.unsigned_tx.compute_txid(),
                    vout: 0,
                },
                script_sig: Default::default(),
                sequence,
                witness: Default::default(),
            })
            .collect(),
        output: outputs,
    };

    let mut unsigned_ark_psbt =
        Psbt::from_unsigned_tx(unsigned_ark_tx).map_err(Error::transaction)?;

    for (i, (checkpoint_psbt, checkpoint_spend_info)) in checkpoint_data.iter().enumerate() {
        // Set checkpoint output as `witness_utxo` field.

        unsigned_ark_psbt.inputs[i].witness_utxo =
            Some(checkpoint_psbt.unsigned_tx.output[0].clone());

        // Set script to be used in `tap_scripts` field for spending the checkpoint output.

        let vtxo_spend_script = &vtxo_inputs[i].spend_script;
        let leaf_version = LeafVersion::TapScript;
        let control_block = checkpoint_spend_info
            .spend_info
            .control_block(&(vtxo_spend_script.clone(), leaf_version))
            .expect("control block for vtxo spend script");

        unsigned_ark_psbt.inputs[i].tap_scripts =
            BTreeMap::from_iter([(control_block, (vtxo_spend_script.clone(), leaf_version))]);

        // Add _all_ the scripts in the Taproot tree to custom unknown field.

        let mut bytes = Vec::new();

        let spend_script = &vtxo_inputs[i].spend_script;
        let scripts = [spend_script.clone(), checkpoint_script.clone()];

        for script in scripts {
            // Write the depth (always 1). TODO: Support more depth.
            bytes.push(1);

            // TODO: Support future leaf versions.
            bytes.push(LeafVersion::TapScript.to_consensus());

            let mut script_bytes = script.to_bytes();

            write_compact_size_uint(&mut bytes, script_bytes.len() as u64)
                .map_err(Error::transaction)?;

            bytes.append(&mut script_bytes);
        }

        unsigned_ark_psbt.inputs[i].unknown.insert(
            psbt::raw::Key {
                type_value: 222,
                key: VTXO_TAPROOT_KEY.to_vec(),
            },
            bytes,
        );
        unsigned_ark_psbt.inputs[i].witness_script = Some(spend_script.clone());
    }

    Ok(OffchainTransactions {
        ark_tx: unsigned_ark_psbt,
        checkpoint_txs: checkpoint_data.into_iter().map(|(psbt, _)| psbt).collect(),
    })
}

#[derive(Debug, Clone)]
struct CheckpointSpendInfo {
    spend_info: TaprootSpendInfo,
}

impl CheckpointSpendInfo {
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

        Self { spend_info }
    }

    fn script_pubkey(&self) -> ScriptBuf {
        tr_script_pubkey(&self.spend_info)
    }
}

fn build_checkpoint_psbt(
    vtxo_input: &VtxoInput,
    // An alternative way for the _server_ to unilaterally spend the checkpoint output, in case the
    // owner does not spend it.
    //
    // This is defined by the Ark server.
    checkpoint_exit_script: ScriptBuf,
) -> Result<(Psbt, CheckpointSpendInfo), Error> {
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

    let checkpoint_spend_info = CheckpointSpendInfo::new(vtxo_input, checkpoint_exit_script);

    let outputs = vec![
        TxOut {
            value: vtxo_input.amount,
            script_pubkey: checkpoint_spend_info.script_pubkey(),
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

    // Set VTXO being spent as `witness_utxo` field.

    unsigned_checkpoint_psbt.inputs[0].witness_utxo = Some(TxOut {
        value: vtxo_input.amount,
        script_pubkey: vtxo_input.script_pubkey.clone(),
    });

    // Set script to be used in `tap_scripts` field for spending the VTXO.

    let (vtxo_spend_script, vtxo_spend_control_block) = vtxo_input.spend_info();

    let leaf_version = vtxo_spend_control_block.leaf_version;
    unsigned_checkpoint_psbt.inputs[0].tap_scripts = BTreeMap::from_iter([(
        vtxo_spend_control_block.clone(),
        (vtxo_spend_script.clone(), leaf_version),
    )]);

    // Add _all_ the scripts in the Taproot tree to custom unknown field.

    let mut bytes = Vec::new();

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

    unsigned_checkpoint_psbt.inputs[0].unknown.insert(
        psbt::raw::Key {
            type_value: 222,
            key: VTXO_TAPROOT_KEY.to_vec(),
        },
        bytes,
    );
    unsigned_checkpoint_psbt.inputs[0].witness_script = Some(vtxo_spend_script.clone());

    Ok((unsigned_checkpoint_psbt, checkpoint_spend_info))
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

// TODO: Sign checkpoint and sign Ark are basically the same. We can combine them, probably.
pub fn sign_checkpoint_transaction<S>(sign_fn: S, psbt: &mut Psbt) -> Result<(), Error>
where
    S: FnOnce(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, Error>,
{
    let witness_utxo = [psbt.inputs[0].witness_utxo.clone().expect("witness UTXO")];
    let prevouts = Prevouts::All(&witness_utxo);

    let psbt_input = psbt.inputs.get_mut(0).expect("input at index");

    let (_, (vtxo_spend_script, leaf_version)) =
        psbt_input.tap_scripts.first_key_value().expect("one entry");

    let leaf_hash = TapLeafHash::from_script(vtxo_spend_script, *leaf_version);

    let tap_sighash = SighashCache::new(&psbt.unsigned_tx)
        .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::Default)
        .map_err(Error::crypto)
        .context("failed to generate sighash")?;

    let msg = secp256k1::Message::from_digest(tap_sighash.to_raw_hash().to_byte_array());

    let sigs = sign_fn(psbt_input, msg)?;
    for (sig, pk) in sigs {
        let sig = taproot::Signature {
            signature: sig,
            sighash_type: TapSighashType::Default,
        };

        psbt_input.tap_script_sigs.insert((pk, leaf_hash), sig);
    }

    Ok(())
}

pub fn sign_ark_transaction<S>(sign_fn: S, psbt: &mut Psbt, input_index: usize) -> Result<(), Error>
where
    S: FnOnce(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, Error>,
{
    tracing::debug!(index = input_index, "Signing Ark transaction input");

    let witness_utxos = psbt
        .inputs
        .iter()
        .map(|i| i.witness_utxo.clone().expect("witness UTXO"))
        .collect::<Vec<_>>();

    let psbt_input = psbt.inputs.get_mut(input_index).expect("input at index");

    // To spend a checkpoint output we are using a script spend path.

    let prevouts = Prevouts::All(&witness_utxos);

    let (_, (vtxo_spend_script, leaf_version)) =
        psbt_input.tap_scripts.first_key_value().expect("one entry");

    let leaf_hash = TapLeafHash::from_script(vtxo_spend_script, *leaf_version);

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

    let sigs = sign_fn(psbt_input, msg)?;
    for (sig, pk) in sigs {
        let sig = taproot::Signature {
            signature: sig,
            sighash_type: TapSighashType::Default,
        };

        psbt_input.tap_script_sigs.insert((pk, leaf_hash), sig);
    }

    Ok(())
}

/// Build unsigned offchain transactions for sending BTC and optional assets to one or more
/// receivers.
///
/// We first build the BTC transaction skeleton via [`build_offchain_transactions`] and then, if the
/// transfer actually involves assets, add exactly one asset packet that:
///
/// - routes each requested asset amount to the corresponding receiver output index
/// - preserves leftover carried assets on the BTC change output
///
/// Specialized flows such as issuance, reissuance, and burn should call
/// [`build_offchain_transactions`] directly and attach their own packet semantics explicitly.
///
/// # Errors
///
/// Returns an error if BTC transaction construction fails, if a receiver references an asset that
/// is not present in the selected inputs, if the requested amount for any asset exceeds the
/// selected input amount for that asset, or if leftover assets would need to be preserved but the
/// transaction has no BTC change output.
pub fn build_asset_send_transactions(
    receivers: &[SendReceiver],
    change_address: &ArkAddress,
    vtxo_inputs: &[VtxoInput],
    server_info: &server::Info,
) -> Result<OffchainTransactions, Error> {
    let mut offchain =
        build_offchain_transactions(receivers, change_address, vtxo_inputs, server_info)?;

    if let Some(packet) = create_send_packet(vtxo_inputs, receivers, &offchain.ark_tx)? {
        add_asset_packet_to_psbt(&mut offchain.ark_tx, &packet);
    }

    Ok(offchain)
}

/// Create the asset packet for a generic asset send.
///
/// Receiver asset allocations are assigned to their corresponding receiver output indexes. Any
/// leftover carried assets are preserved on the BTC change output when one exists.
fn create_send_packet(
    inputs: &[VtxoInput],
    receivers: &[SendReceiver],
    ark_tx: &Psbt,
) -> Result<Option<asset::packet::Packet>, Error> {
    struct AssetTransfer {
        inputs: Vec<asset::packet::AssetInput>,
        outputs: Vec<asset::packet::AssetOutput>,
        input_amount: u64,
        requested_amount: u64,
    }

    let mut transfers: HashMap<AssetId, AssetTransfer> = HashMap::new();

    for (input_index, input) in inputs.iter().enumerate() {
        for asset in &input.assets {
            let transfer = transfers
                .entry(asset.asset_id)
                .or_insert_with(|| AssetTransfer {
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                    input_amount: 0,
                    requested_amount: 0,
                });

            transfer.inputs.push(asset::packet::AssetInput {
                input_index: input_index as u16,
                amount: asset.amount,
            });
            transfer.input_amount += asset.amount;
        }
    }

    let any_receiver_assets = receivers.iter().any(|receiver| !receiver.assets.is_empty());
    if transfers.is_empty() && !any_receiver_assets {
        return Ok(None);
    }

    for (receiver_index, receiver) in receivers.iter().enumerate() {
        for asset in &receiver.assets {
            let transfer = transfers.get_mut(&asset.asset_id).ok_or_else(|| {
                Error::ad_hoc(format!(
                    "receiver references asset {} that is not present in selected inputs",
                    asset.asset_id
                ))
            })?;

            transfer.outputs.push(asset::packet::AssetOutput {
                output_index: receiver_index as u16,
                amount: asset.amount,
            });
            transfer.requested_amount = transfer
                .requested_amount
                .checked_add(asset.amount)
                .ok_or_else(|| Error::ad_hoc("asset transfer amount overflow"))?;
        }
    }

    let change_output_index = btc_change_output_index(ark_tx, receivers.len());
    let mut groups = Vec::new();

    for (asset_id, mut transfer) in transfers.into_iter() {
        let leftover_amount = transfer
            .input_amount
            .checked_sub(transfer.requested_amount)
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "requested amount for asset {} exceeds selected input amount",
                    asset_id
                ))
            })?;

        match (change_output_index, leftover_amount) {
            (Some(change_output_index), leftover_amount) if leftover_amount > 0 => {
                transfer.outputs.push(asset::packet::AssetOutput {
                    output_index: change_output_index,
                    amount: leftover_amount,
                });
            }
            (None, leftover_amount) if leftover_amount > 0 => {
                return Err(Error::ad_hoc(
                    "asset transfer has preserved asset changes but no BTC change output",
                ));
            }
            _ => {}
        }

        groups.push(asset::packet::AssetGroup {
            asset_id: Some(asset_id),
            control_asset: None,
            metadata: None,
            inputs: transfer.inputs,
            outputs: transfer.outputs,
        });
    }

    groups.sort_by_key(|group| {
        let asset_id = group
            .asset_id
            .expect("generic asset-send groups always have asset ids");
        (*asset_id.txid.as_byte_array(), asset_id.group_index)
    });

    Ok(Some(asset::packet::Packet { groups }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::packet::AssetGroup;
    use crate::asset::packet::AssetInput;
    use crate::asset::packet::AssetOutput;
    use crate::asset::packet::Packet;
    use crate::script::multisig_script;
    use crate::send::VtxoInput;
    use crate::server::Info;
    use bitcoin::key::Secp256k1;
    use bitcoin::opcodes::OP_TRUE;
    use bitcoin::script::Builder;
    use bitcoin::taproot::LeafVersion;
    use bitcoin::taproot::TaprootBuilder;
    use bitcoin::Amount;
    use bitcoin::Network;
    use bitcoin::OutPoint;
    use bitcoin::Sequence;
    use bitcoin::Txid;

    #[test]
    fn build_offchain_transactions_has_no_packet_even_when_assets_are_present() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([10; 32]),
            group_index: 0,
        };
        let (input, own_address) = asset_send_input(
            1,
            660,
            vec![Asset {
                asset_id,
                amount: 10,
            }],
        );
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![Asset {
                asset_id,
                amount: 6,
            }],
        };

        let res =
            build_offchain_transactions(&[receiver], &own_address, &[input], &server_info).unwrap();

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 3);
    }

    #[test]
    fn build_asset_send_transactions_routes_requested_assets_to_receiver_outputs_and_change() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([11; 32]),
            group_index: 4,
        };
        let (input, own_address) = asset_send_input(
            2,
            660,
            vec![Asset {
                asset_id,
                amount: 10,
            }],
        );
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![Asset {
                asset_id,
                amount: 6,
            }],
        };

        let res = build_asset_send_transactions(&[receiver], &own_address, &[input], &server_info)
            .unwrap();

        let expected_packet = Packet {
            groups: vec![AssetGroup {
                asset_id: Some(asset_id),
                control_asset: None,
                metadata: None,
                inputs: vec![AssetInput {
                    input_index: 0,
                    amount: 10,
                }],
                outputs: vec![
                    AssetOutput {
                        output_index: 0,
                        amount: 6,
                    },
                    AssetOutput {
                        output_index: 1,
                        amount: 4,
                    },
                ],
            }],
        };

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index(&res.ark_tx)],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn build_asset_send_transactions_errors_when_receiver_references_missing_asset() {
        let server_info = test_server_info();
        let missing_asset_id = AssetId {
            txid: Txid::from_byte_array([12; 32]),
            group_index: 1,
        };
        let (input, own_address) = asset_send_input(3, 330, vec![]);
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![Asset {
                asset_id: missing_asset_id,
                amount: 1,
            }],
        };

        let err = build_asset_send_transactions(&[receiver], &own_address, &[input], &server_info)
            .unwrap_err();

        assert!(err.to_string().contains("receiver references asset"));
    }

    #[test]
    fn build_asset_send_transactions_errors_when_leftover_assets_exist_but_no_btc_change_output() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([13; 32]),
            group_index: 2,
        };
        let (input, own_address) = asset_send_input(
            4,
            330,
            vec![Asset {
                asset_id,
                amount: 10,
            }],
        );
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![Asset {
                asset_id,
                amount: 6,
            }],
        };

        let err = build_asset_send_transactions(&[receiver], &own_address, &[input], &server_info)
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("asset transfer has preserved asset changes but no BTC change output"));
    }

    #[test]
    fn build_asset_send_transactions_sorts_packet_groups_stably() {
        let server_info = test_server_info();
        let asset_id_a = AssetId {
            txid: Txid::from_byte_array([14; 32]),
            group_index: 1,
        };
        let asset_id_b = AssetId {
            txid: Txid::from_byte_array([15; 32]),
            group_index: 0,
        };
        let (input, own_address) = asset_send_input(
            5,
            660,
            vec![
                Asset {
                    asset_id: asset_id_b,
                    amount: 8,
                },
                Asset {
                    asset_id: asset_id_a,
                    amount: 10,
                },
            ],
        );
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![
                Asset {
                    asset_id: asset_id_b,
                    amount: 3,
                },
                Asset {
                    asset_id: asset_id_a,
                    amount: 4,
                },
            ],
        };

        let res = build_asset_send_transactions(&[receiver], &own_address, &[input], &server_info)
            .unwrap();

        let expected_packet = Packet {
            groups: vec![
                AssetGroup {
                    asset_id: Some(asset_id_a),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![AssetInput {
                        input_index: 0,
                        amount: 10,
                    }],
                    outputs: vec![
                        AssetOutput {
                            output_index: 0,
                            amount: 4,
                        },
                        AssetOutput {
                            output_index: 1,
                            amount: 6,
                        },
                    ],
                },
                AssetGroup {
                    asset_id: Some(asset_id_b),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![AssetInput {
                        input_index: 0,
                        amount: 8,
                    }],
                    outputs: vec![
                        AssetOutput {
                            output_index: 0,
                            amount: 3,
                        },
                        AssetOutput {
                            output_index: 1,
                            amount: 5,
                        },
                    ],
                },
            ],
        };

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index(&res.ark_tx)],
            expected_packet.to_txout()
        );
    }

    fn test_server_info() -> Info {
        let signer_pk = "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0"
            .parse()
            .unwrap();
        let forfeit_pk = "03dff1d77f2a671c5f36183726db2341be58f8be17d2a3d1d2cd47b7b0f5f2d624"
            .parse()
            .unwrap();

        Info {
            version: "test".into(),
            signer_pk,
            forfeit_pk,
            forfeit_address: "bcrt1q8frde3yn78tl9ecgq4anlz909jh0clefhucdur"
                .parse::<bitcoin::Address<_>>()
                .unwrap()
                .require_network(Network::Regtest)
                .unwrap(),
            checkpoint_tapscript: Builder::new().push_opcode(OP_TRUE).into_script(),
            network: Network::Regtest,
            session_duration: 0,
            unilateral_exit_delay: Sequence::MAX,
            boarding_exit_delay: Sequence::MAX,
            utxo_min_amount: None,
            utxo_max_amount: None,
            vtxo_min_amount: Some(Amount::from_sat(1)),
            vtxo_max_amount: None,
            dust: Amount::from_sat(330),
            fees: None,
            scheduled_session: None,
            deprecated_signers: vec![],
            service_status: Default::default(),
            digest: "test".into(),
        }
    }

    fn asset_send_input(
        outpoint_tag: u8,
        amount_sat: u64,
        assets: Vec<Asset>,
    ) -> (VtxoInput, ArkAddress) {
        let secp = Secp256k1::new();

        let server_pk: PublicKey =
            "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0"
                .parse()
                .unwrap();
        let owner_pk: PublicKey =
            "03dff1d77f2a671c5f36183726db2341be58f8be17d2a3d1d2cd47b7b0f5f2d624"
                .parse()
                .unwrap();

        let server_xonly = server_pk.inner.x_only_public_key().0;
        let owner_xonly = owner_pk.inner.x_only_public_key().0;
        let spend_script = multisig_script(server_xonly, owner_xonly);
        let spend_info = TaprootBuilder::new()
            .add_leaf(0, spend_script.clone())
            .unwrap()
            .finalize(&secp, server_xonly)
            .unwrap();
        let control_block = spend_info
            .control_block(&(spend_script.clone(), LeafVersion::TapScript))
            .unwrap();
        let own_address = ArkAddress::new(Network::Regtest, server_xonly, spend_info.output_key());

        (
            VtxoInput::new(
                spend_script.clone(),
                None,
                control_block,
                vec![spend_script],
                own_address.to_p2tr_script_pubkey(),
                Amount::from_sat(amount_sat),
                OutPoint::new(Txid::from_byte_array([outpoint_tag; 32]), 0),
                assets,
            ),
            own_address,
        )
    }

    fn asset_packet_index(ark_tx: &Psbt) -> usize {
        ark_tx.unsigned_tx.output.len() - 2
    }
}
