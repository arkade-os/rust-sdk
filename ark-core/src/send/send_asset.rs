use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::send::btc_change_output_index;
use crate::send::build_offchain_transactions;
use crate::send::AssetBearingVtxoInput;
use crate::send::OffchainTransactions;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use bitcoin::hashes::Hash as _;
use bitcoin::Amount;
use bitcoin::Psbt;
use std::collections::HashMap;

/// A receiver for a generic offchain send with optional assets.
#[derive(Debug, Clone)]
pub struct SendReceiver {
    pub address: ArkAddress,
    pub amount: Amount,
    pub assets: Vec<server::Asset>,
}

/// Unsigned transactions for a generic offchain send.
#[derive(Debug, Clone)]
pub struct SendTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build unsigned offchain transactions for sending BTC and optional assets to one or more
/// receivers.
///
/// Receiver outputs are assigned in the same order as `receivers`. Any assets left over after
/// satisfying the requested receiver allocations are preserved on the BTC change output.
///
/// # Arguments
///
/// * `receivers` - Offchain recipients and the BTC/asset amounts assigned to each transaction
///   output
/// * `change_address` - The sender's offchain change address, used if the transaction has BTC
///   change
/// * `inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `server_info` - Server configuration used to build the offchain transaction shape and dust
///   output
///
/// # Returns
///
/// [`SendTransactions`] containing the unsigned Ark transaction and unsigned checkpoint
/// transactions.
///
/// # Errors
///
/// Returns an error if unsigned offchain transaction construction fails, if a receiver references
/// an asset that is not present in the selected inputs, if the requested amount for any asset
/// exceeds the selected input amount for that asset, or if leftover assets would need to be
/// preserved but the transaction has no BTC change output.
pub fn build_send_transactions(
    receivers: &[SendReceiver],
    change_address: &ArkAddress,
    inputs: &[AssetBearingVtxoInput],
    server_info: &server::Info,
) -> Result<SendTransactions, Error> {
    let vtxo_inputs = inputs
        .iter()
        .map(|input| input.input.clone())
        .collect::<Vec<_>>();
    let btc_outputs = receivers
        .iter()
        .map(|receiver| (&receiver.address, receiver.amount))
        .collect::<Vec<_>>();

    let OffchainTransactions {
        mut ark_tx,
        checkpoint_txs,
    } = build_offchain_transactions(
        &btc_outputs,
        Some(change_address),
        &vtxo_inputs,
        server_info,
    )?;

    if let Some(packet) = create_send_packet(inputs, receivers, &ark_tx)? {
        add_asset_packet_to_psbt(&mut ark_tx, &packet);
    }

    Ok(SendTransactions {
        ark_tx,
        checkpoint_txs,
    })
}

/// Create the asset packet for a generic asset send.
///
/// Receiver asset allocations are assigned to their corresponding receiver output indexes. Any
/// leftover carried assets are preserved on the BTC change output when one exists.
///
/// # Errors
///
/// Returns an error if a receiver references an asset that is not present in the selected inputs,
/// if the requested amount for any asset exceeds the selected input amount for that asset, or if
/// leftover assets would need to be preserved but the transaction has no BTC change output.
fn create_send_packet(
    inputs: &[AssetBearingVtxoInput],
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
    use crate::server::Asset;
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
    fn asset_send_without_assets_has_no_packet() {
        let server_info = test_server_info();
        let (input, own_address) = asset_send_input(1, 330, vec![]);
        let receiver = SendReceiver {
            address: own_address,
            amount: Amount::from_sat(330),
            assets: vec![],
        };

        let res =
            build_send_transactions(&[receiver], &own_address, &[input], &server_info).unwrap();

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 2);
    }

    #[test]
    fn asset_send_routes_requested_assets_to_receiver_outputs_and_change() {
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

        let res =
            build_send_transactions(&[receiver], &own_address, &[input], &server_info).unwrap();

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
    fn asset_send_errors_when_receiver_references_missing_asset() {
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

        let err =
            build_send_transactions(&[receiver], &own_address, &[input], &server_info).unwrap_err();

        assert!(err.to_string().contains("receiver references asset"));
    }

    #[test]
    fn asset_send_errors_when_leftover_assets_exist_but_no_btc_change_output() {
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

        let err =
            build_send_transactions(&[receiver], &own_address, &[input], &server_info).unwrap_err();

        assert!(err
            .to_string()
            .contains("asset transfer has preserved asset changes but no BTC change output"));
    }

    #[test]
    fn asset_send_sorts_packet_groups_stably() {
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

        let res =
            build_send_transactions(&[receiver], &own_address, &[input], &server_info).unwrap();

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
    ) -> (AssetBearingVtxoInput, ArkAddress) {
        let secp = Secp256k1::new();

        let server_pk: bitcoin::key::PublicKey =
            "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0"
                .parse()
                .unwrap();
        let owner_pk: bitcoin::key::PublicKey =
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
            AssetBearingVtxoInput {
                input: VtxoInput::new(
                    spend_script.clone(),
                    None,
                    control_block,
                    vec![spend_script],
                    own_address.to_p2tr_script_pubkey(),
                    Amount::from_sat(amount_sat),
                    OutPoint::new(Txid::from_byte_array([outpoint_tag; 32]), 0),
                ),
                assets,
            },
            own_address,
        )
    }

    fn asset_packet_index(ark_tx: &Psbt) -> usize {
        ark_tx.unsigned_tx.output.len() - 2
    }
}
