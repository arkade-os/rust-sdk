use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::send::build_offchain_transactions;
use crate::send::has_btc_change_output;
use crate::send::preserved_asset_output_index;
use crate::send::AssetBearingVtxoInput;
use crate::send::OffchainTransactions;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use bitcoin::Psbt;
use std::collections::HashMap;

/// Unsigned transactions for asset reissuance.
#[derive(Debug, Clone)]
pub struct AssetReissuanceTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build unsigned offchain transactions for reissuing an existing asset to self.
///
/// Output `0` remains self-controlled and carries both the returned control asset and the newly
/// reissued asset amount.
///
/// Assets already carried by the selected inputs are preserved on the BTC change output when one
/// exists. If no BTC change output exists, the builder returns an error rather than silently
/// dropping them.
///
/// # Arguments
///
/// * `own_address` - The issuer's offchain address that receives the returned control asset and the
///   newly reissued asset amount
/// * `change_address` - The issuer's offchain change address, used if the transaction has BTC
///   change
/// * `inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `server_info` - Server configuration used to build the offchain transaction shape and dust
///   output
/// * `reissue_asset_id` - The ID of the existing asset being reissued
/// * `control_asset_id` - The ID of the control asset authorizing the reissuance
/// * `reissue_amount` - The additional amount of the asset to mint
///
/// # Returns
///
/// [`AssetReissuanceTransactions`] containing the unsigned Ark transaction and unsigned checkpoint
/// transactions.
pub fn build_asset_reissuance_transactions(
    own_address: &ArkAddress,
    change_address: &ArkAddress,
    inputs: &[AssetBearingVtxoInput],
    server_info: &server::Info,
    reissue_asset_id: AssetId,
    control_asset_id: AssetId,
    reissue_amount: u64,
) -> Result<AssetReissuanceTransactions, Error> {
    if reissue_amount == 0 {
        return Err(Error::ad_hoc("reissue amount must be > 0"));
    }

    let vtxo_inputs = inputs
        .iter()
        .map(|input| input.input.clone())
        .collect::<Vec<_>>();

    let OffchainTransactions {
        mut ark_tx,
        checkpoint_txs,
    } = build_offchain_transactions(
        &[(own_address, server_info.dust)],
        Some(change_address),
        &vtxo_inputs,
        server_info,
    )?;

    // We have to make sure that assets that were already associated with the provided inputs are
    // preserved.
    //
    // TODO: Review whether reissuance should mirror self-issuance here. Today we error if preserved
    // asset changes exist but there is no BTC change output, to avoid silently dropping them. Since
    // output 0 is also self-controlled in reissuance, it may be valid to preserve those assets on
    // output 0 instead.
    let (asset_inputs, change_assets) = derive_reissuance_assets(inputs, control_asset_id);

    let can_preserve_existing_assets = has_btc_change_output(&ark_tx, 1);
    match (can_preserve_existing_assets, change_assets.is_empty()) {
        (true, _) | (false, true) => {}
        (false, false) => {
            return Err(Error::ad_hoc(
                "asset reissuance has preserved asset changes but no BTC change output",
            ));
        }
    }

    let mut packet = create_asset_packet(
        &asset_inputs,
        &[vec![server::Asset {
            asset_id: control_asset_id,
            amount: 1,
        }]],
        &change_assets,
        preserved_asset_output_index(&ark_tx, 1) as usize,
    )?
    .unwrap_or_else(|| asset::packet::Packet { groups: Vec::new() });

    let reissue_output = asset::packet::AssetOutput {
        output_index: 0,
        amount: reissue_amount,
    };

    if let Some(group) = packet.groups.iter_mut().find(|group| {
        group
            .asset_id
            .as_ref()
            .map(|id| *id == reissue_asset_id)
            .unwrap_or(false)
    }) {
        group.outputs.push(reissue_output);
    } else {
        packet.groups.push(asset::packet::AssetGroup {
            asset_id: Some(reissue_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![],
            outputs: vec![reissue_output],
        });
    }

    add_asset_packet_to_psbt(&mut ark_tx, &packet);

    Ok(AssetReissuanceTransactions {
        ark_tx,
        checkpoint_txs,
    })
}

fn derive_reissuance_assets(
    inputs: &[AssetBearingVtxoInput],
    control_asset_id: AssetId,
) -> (HashMap<u16, Vec<server::Asset>>, Vec<server::Asset>) {
    let mut asset_inputs = HashMap::new();
    let mut change_assets: HashMap<AssetId, u64> = HashMap::new();
    let mut control_asset_amount = 0;

    for (input_index, input) in inputs.iter().enumerate() {
        if !input.assets.is_empty() {
            asset_inputs.insert(input_index as u16, input.assets.clone());
        }

        for asset in &input.assets {
            if asset.asset_id == control_asset_id {
                control_asset_amount += asset.amount;
            } else {
                *change_assets.entry(asset.asset_id).or_insert(0) += asset.amount;
            }
        }
    }

    if control_asset_amount > 1 {
        *change_assets.entry(control_asset_id).or_insert(0) += control_asset_amount - 1;
    }

    let change_assets = change_assets
        .into_iter()
        .map(|(asset_id, amount)| server::Asset { asset_id, amount })
        .collect();

    (asset_inputs, change_assets)
}

fn create_asset_packet(
    asset_inputs: &HashMap<u16, Vec<server::Asset>>,
    receiver_assets: &[Vec<server::Asset>],
    change_assets: &[server::Asset],
    change_output_index: usize,
) -> Result<Option<asset::packet::Packet>, Error> {
    struct AssetTransfer {
        inputs: Vec<asset::packet::AssetInput>,
        outputs: Vec<asset::packet::AssetOutput>,
    }

    let mut transfers: HashMap<AssetId, AssetTransfer> = HashMap::new();

    for (input_index, assets) in asset_inputs {
        for asset in assets {
            let transfer = transfers
                .entry(asset.asset_id)
                .or_insert_with(|| AssetTransfer {
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                });
            transfer.inputs.push(asset::packet::AssetInput {
                input_index: *input_index,
                amount: asset.amount,
            });
        }
    }

    for (receiver_index, receiver_assets) in receiver_assets.iter().enumerate() {
        for asset in receiver_assets {
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
        }
    }

    for asset in change_assets {
        if let Some(transfer) = transfers.get_mut(&asset.asset_id) {
            transfer.outputs.push(asset::packet::AssetOutput {
                output_index: change_output_index as u16,
                amount: asset.amount,
            });
        }
    }

    if transfers.is_empty() {
        return Ok(None);
    }

    Ok(Some(asset::packet::Packet {
        groups: transfers
            .into_iter()
            .map(|(asset_id, transfer)| asset::packet::AssetGroup {
                asset_id: Some(asset_id),
                control_asset: None,
                metadata: None,
                inputs: transfer.inputs,
                outputs: transfer.outputs,
            })
            .collect(),
    }))
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
    use bitcoin::hashes::Hash as _;
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
    fn self_reissuance_returns_control_asset_and_mints_reissued_asset() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([1; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([2; 32]),
            group_index: 1,
        };
        let (input, own_address) = self_reissuance_input(
            3,
            330,
            vec![Asset {
                asset_id: control_asset_id,
                amount: 1,
            }],
        );

        let res = build_asset_reissuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            asset_id,
            control_asset_id,
            123,
        )
        .unwrap();

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 3);

        let expected_packet = Packet {
            groups: vec![
                AssetGroup {
                    asset_id: Some(control_asset_id),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![AssetInput {
                        input_index: 0,
                        amount: 1,
                    }],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 1,
                    }],
                },
                AssetGroup {
                    asset_id: Some(asset_id),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 123,
                    }],
                },
            ],
        };

        assert_eq!(res.ark_tx.unsigned_tx.output[1], expected_packet.to_txout());
    }

    #[test]
    fn self_reissuance_with_btc_change_preserves_asset_changes_on_change_output() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([4; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([5; 32]),
            group_index: 1,
        };
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([6; 32]),
            group_index: 2,
        };
        let (input, own_address) = self_reissuance_input(
            7,
            660,
            vec![
                Asset {
                    asset_id: control_asset_id,
                    amount: 1,
                },
                Asset {
                    asset_id: unrelated_asset_id,
                    amount: 9,
                },
            ],
        );

        let res = build_asset_reissuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            asset_id,
            control_asset_id,
            123,
        )
        .unwrap();

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 4);

        let control_group = AssetGroup {
            asset_id: Some(control_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 1,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: 1,
            }],
        };
        let unrelated_group = AssetGroup {
            asset_id: Some(unrelated_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 9,
            }],
            outputs: vec![AssetOutput {
                output_index: 1,
                amount: 9,
            }],
        };
        let reissued_group = AssetGroup {
            asset_id: Some(asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: 123,
            }],
        };
        let expected_packets = [
            Packet {
                groups: vec![
                    control_group.clone(),
                    unrelated_group.clone(),
                    reissued_group.clone(),
                ],
            },
            Packet {
                groups: vec![
                    control_group.clone(),
                    reissued_group.clone(),
                    unrelated_group.clone(),
                ],
            },
            Packet {
                groups: vec![
                    unrelated_group.clone(),
                    control_group.clone(),
                    reissued_group.clone(),
                ],
            },
            Packet {
                groups: vec![
                    unrelated_group.clone(),
                    reissued_group.clone(),
                    control_group.clone(),
                ],
            },
            Packet {
                groups: vec![
                    reissued_group.clone(),
                    control_group.clone(),
                    unrelated_group.clone(),
                ],
            },
            Packet {
                groups: vec![reissued_group, unrelated_group, control_group],
            },
        ];

        assert!(expected_packets
            .iter()
            .any(|packet| res.ark_tx.unsigned_tx.output[2] == packet.to_txout()));
    }

    #[test]
    fn self_reissuance_with_existing_asset_balance_preserves_it_on_change_output() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([8; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([9; 32]),
            group_index: 1,
        };
        let (input, own_address) = self_reissuance_input(
            10,
            660,
            vec![
                Asset {
                    asset_id: control_asset_id,
                    amount: 1,
                },
                Asset {
                    asset_id,
                    amount: 7,
                },
            ],
        );

        let res = build_asset_reissuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            asset_id,
            control_asset_id,
            123,
        )
        .unwrap();

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 4);

        let expected_packets = [
            Packet {
                groups: vec![
                    AssetGroup {
                        asset_id: Some(control_asset_id),
                        control_asset: None,
                        metadata: None,
                        inputs: vec![AssetInput {
                            input_index: 0,
                            amount: 1,
                        }],
                        outputs: vec![AssetOutput {
                            output_index: 0,
                            amount: 1,
                        }],
                    },
                    AssetGroup {
                        asset_id: Some(asset_id),
                        control_asset: None,
                        metadata: None,
                        inputs: vec![AssetInput {
                            input_index: 0,
                            amount: 7,
                        }],
                        outputs: vec![
                            AssetOutput {
                                output_index: 1,
                                amount: 7,
                            },
                            AssetOutput {
                                output_index: 0,
                                amount: 123,
                            },
                        ],
                    },
                ],
            },
            Packet {
                groups: vec![
                    AssetGroup {
                        asset_id: Some(asset_id),
                        control_asset: None,
                        metadata: None,
                        inputs: vec![AssetInput {
                            input_index: 0,
                            amount: 7,
                        }],
                        outputs: vec![
                            AssetOutput {
                                output_index: 1,
                                amount: 7,
                            },
                            AssetOutput {
                                output_index: 0,
                                amount: 123,
                            },
                        ],
                    },
                    AssetGroup {
                        asset_id: Some(control_asset_id),
                        control_asset: None,
                        metadata: None,
                        inputs: vec![AssetInput {
                            input_index: 0,
                            amount: 1,
                        }],
                        outputs: vec![AssetOutput {
                            output_index: 0,
                            amount: 1,
                        }],
                    },
                ],
            },
        ];

        assert!(expected_packets
            .iter()
            .any(|packet| res.ark_tx.unsigned_tx.output[2] == packet.to_txout()));
    }

    #[test]
    fn self_reissuance_without_btc_change_errors_when_asset_changes_would_be_dropped() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([4; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([5; 32]),
            group_index: 1,
        };
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([6; 32]),
            group_index: 2,
        };
        let (input, own_address) = self_reissuance_input(
            7,
            330,
            vec![
                Asset {
                    asset_id: control_asset_id,
                    amount: 1,
                },
                Asset {
                    asset_id: unrelated_asset_id,
                    amount: 9,
                },
            ],
        );

        let err = build_asset_reissuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            asset_id,
            control_asset_id,
            123,
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("asset reissuance has preserved asset changes but no BTC change output"));
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

    fn self_reissuance_input(
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
}
