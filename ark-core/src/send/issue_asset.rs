use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::asset::ControlAssetConfig;
use crate::send::build_offchain_transactions;
use crate::send::OffchainTransactions;
use crate::send::SendReceiver;
use crate::send::VtxoInput;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use bitcoin::hashes::Hash as _;
use bitcoin::Psbt;
use bitcoin::Txid;
use std::collections::HashMap;

/// Unsigned transactions for self asset issuance plus the derived asset IDs.
#[derive(Debug, Clone)]
pub struct SelfAssetIssuanceTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
    pub asset_ids: Vec<AssetId>,
}

/// Self-controlled output used by self-issuance packet assignments.
const SELF_ISSUANCE_OUTPUT_INDEX: u16 = 0;

/// Build unsigned offchain transactions for issuing a fresh asset to self.
///
/// Output [`SELF_ISSUANCE_OUTPUT_INDEX`] remains self-controlled and carries the newly issued
/// asset amount together with any assets already carried by the selected inputs.
///
/// This builder is therefore intended for self-issuance flows, where output
/// [`SELF_ISSUANCE_OUTPUT_INDEX`] remains under the issuer's control.
///
/// # Arguments
///
/// * `own_address` - The issuer's offchain address that receives the newly issued asset
/// * `change_address` - The issuer's offchain change address, used if the transaction has BTC
///   change
/// * `inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `server_info` - Server configuration used to build the offchain transaction shape and dust
///   output
/// * `amount` - The amount of the new asset to issue
/// * `control_asset_config` - Optional control asset configuration for making the issued asset
///   reissuable
/// * `metadata` - Optional metadata to attach to the newly issued asset group
///
/// # Returns
///
/// [`SelfAssetIssuanceTransactions`] containing the unsigned Ark transaction, unsigned checkpoint
/// transactions, and the derived asset IDs for the issued asset groups.
///
/// # Errors
///
/// Returns an error if `amount` is zero, if an existing control asset is requested but the
/// selected inputs do not include a non-zero balance of that asset, or if unsigned offchain
/// transaction construction fails.
pub fn build_self_asset_issuance_transactions(
    own_address: &ArkAddress,
    change_address: &ArkAddress,
    inputs: &[VtxoInput],
    server_info: &server::Info,
    amount: u64,
    control_asset_config: Option<ControlAssetConfig>,
    metadata: Option<Vec<(String, String)>>,
) -> Result<SelfAssetIssuanceTransactions, Error> {
    if amount == 0 {
        return Err(Error::ad_hoc("asset amount must be > 0"));
    }

    let OffchainTransactions {
        mut ark_tx,
        checkpoint_txs,
    } = build_offchain_transactions(
        &[SendReceiver {
            address: *own_address,
            amount: server_info.dust,
            assets: Vec::new(),
        }],
        change_address,
        inputs,
        server_info,
    )?;

    let packet = create_self_issuance_packet(
        inputs,
        amount,
        control_asset_config.as_ref(),
        metadata.as_ref(),
    )?;
    add_asset_packet_to_psbt(&mut ark_tx, &packet)?;

    let asset_ids = derive_issued_asset_ids(
        ark_tx.unsigned_tx.compute_txid(),
        control_asset_config.as_ref(),
    );

    Ok(SelfAssetIssuanceTransactions {
        ark_tx,
        checkpoint_txs,
        asset_ids,
    })
}

/// Create the asset packet for a self-issuance transaction.
///
/// Output [`SELF_ISSUANCE_OUTPUT_INDEX`] is treated as the self-controlled destination for the
/// newly issued asset and any assets already carried by the selected inputs.
///
/// If a new control asset is created, it is emitted as group `0` so the issued asset can refer to
/// it via [`asset::packet::AssetRef::ByGroup`]. Carried assets are preserved on output
/// [`SELF_ISSUANCE_OUTPUT_INDEX`] as well.
///
/// # Arguments
///
/// * `inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `amount` - The amount of the new asset to issue
/// * `control_asset_config` - Optional control asset configuration for making the issued asset
///   reissuable
/// * `metadata` - Optional metadata to attach to the newly issued asset group
///
/// # Returns
///
/// An [`asset::packet::Packet`] describing how newly issued and carried assets are assigned to
/// output [`SELF_ISSUANCE_OUTPUT_INDEX`].
///
/// # Errors
///
/// Returns an error if `control_asset_config` references an existing control asset id that is not
/// present with a non-zero balance in the selected inputs.
fn create_self_issuance_packet(
    inputs: &[VtxoInput],
    amount: u64,
    control_asset_config: Option<&ControlAssetConfig>,
    metadata: Option<&Vec<(String, String)>>,
) -> Result<asset::packet::Packet, Error> {
    let mut groups = Vec::new();

    // If we are generating a new control asset as part of issuing, it belongs in group `0`.
    if let Some(ControlAssetConfig::New {
        amount: control_amount,
    }) = control_asset_config
    {
        groups.push(asset::packet::AssetGroup {
            asset_id: None,
            control_asset: None,
            metadata: metadata.cloned(),
            inputs: vec![],
            outputs: vec![asset::packet::AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: (*control_amount).into(),
            }],
        });
    }

    let control_asset_ref = match control_asset_config {
        Some(ControlAssetConfig::New { .. }) => Some(asset::packet::AssetRef::ByGroup(0)),
        Some(ControlAssetConfig::Existing { id }) => Some(asset::packet::AssetRef::ById(*id)),
        None => None,
    };

    // The issued asset can be in either:
    //
    // - group `0`, if no new control asset was generated; or
    // - group `1`, if a new control asset was generated.
    groups.push(asset::packet::AssetGroup {
        asset_id: None,
        control_asset: control_asset_ref,
        metadata: metadata.cloned(),
        inputs: vec![],
        outputs: vec![asset::packet::AssetOutput {
            output_index: SELF_ISSUANCE_OUTPUT_INDEX,
            amount,
        }],
    });

    // Ensure asset preservation.
    let mut existing_asset_groups: HashMap<AssetId, asset::packet::AssetGroup> = HashMap::new();
    for (input_index, input) in inputs.iter().enumerate() {
        for asset in &input.assets {
            let group = existing_asset_groups
                .entry(asset.asset_id)
                .or_insert_with(|| asset::packet::AssetGroup {
                    asset_id: Some(asset.asset_id),
                    control_asset: None,
                    metadata: None,
                    inputs: Vec::new(),
                    outputs: vec![asset::packet::AssetOutput {
                        output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                        amount: 0,
                    }],
                });

            group.inputs.push(asset::packet::AssetInput {
                input_index: input_index as u16,
                amount: asset.amount,
            });

            group.outputs[0].amount = group.outputs[0]
                .amount
                .checked_add(asset.amount)
                .ok_or_else(|| {
                    Error::ad_hoc("asset amount overflow while preserving carried assets")
                })?;
        }
    }

    // If the control asset is referenced by ID (it existed before this transaction), ensure that it
    // is an input to the transaction. Otherwise, the Arkade server will not authorise issuance.
    if let Some(ControlAssetConfig::Existing { id }) = control_asset_config {
        let control_group = existing_asset_groups
            .get(id)
            .map(|t| t.inputs.as_slice())
            .unwrap_or_default();
        let control_input_amount: u64 = control_group.iter().map(|i| i.amount).sum();

        if control_input_amount == 0 {
            return Err(Error::ad_hoc(
                "control asset missing from issuance transaction inputs",
            ));
        }
    }

    // Sort the remaining groups to make it easier to test the behaviour. This is _not_ required.
    let mut existing_asset_groups = existing_asset_groups.into_values().collect::<Vec<_>>();
    existing_asset_groups.sort_by_key(|group| {
        let asset_id = group
            .asset_id
            .expect("issuance carried-asset groups always have asset ids");
        (asset_id.txid.to_byte_array(), asset_id.group_index)
    });
    groups.extend(existing_asset_groups);

    Ok(asset::packet::Packet { groups })
}

/// Derive the asset IDs created by a self-issuance transaction from the final transaction ID.
///
/// When a new control asset is created, it is emitted as group `0` and the issued asset becomes
/// group `1`. Otherwise, the issued asset is group `0`.
fn derive_issued_asset_ids(
    txid: Txid,
    control_asset_config: Option<&ControlAssetConfig>,
) -> Vec<AssetId> {
    let mut asset_ids = Vec::new();
    let mut group_index = 0;

    if matches!(control_asset_config, Some(ControlAssetConfig::New { .. })) {
        asset_ids.push(AssetId { txid, group_index });
        group_index += 1;
    }

    asset_ids.push(AssetId { txid, group_index });
    asset_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::packet::AssetGroup;
    use crate::asset::packet::AssetInput;
    use crate::asset::packet::AssetOutput;
    use crate::asset::packet::Packet;
    use crate::asset::ControlAssetConfig;
    use crate::script::multisig_script;
    use crate::send::VtxoInput;
    use crate::server;
    use crate::server::Asset;
    use crate::ArkAddress;
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
    fn derive_issued_asset_ids_without_control_asset() {
        let txid = Txid::from_byte_array([1; 32]);

        assert_eq!(
            derive_issued_asset_ids(txid, None),
            vec![AssetId {
                txid,
                group_index: 0,
            }]
        );
    }

    #[test]
    fn derive_issued_asset_ids_with_existing_control_asset() {
        let txid = Txid::from_byte_array([2; 32]);
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([3; 32]),
            group_index: 7,
        };

        assert_eq!(
            derive_issued_asset_ids(txid, Some(&ControlAssetConfig::existing(control_asset_id))),
            vec![AssetId {
                txid,
                group_index: 0,
            }]
        );
    }

    #[test]
    fn derive_issued_asset_ids_with_new_control_asset() {
        let txid = Txid::from_byte_array([4; 32]);
        let control_asset = ControlAssetConfig::new(1).expect("non-zero control asset amount");

        assert_eq!(
            derive_issued_asset_ids(txid, Some(&control_asset)),
            vec![
                AssetId {
                    txid,
                    group_index: 0,
                },
                AssetId {
                    txid,
                    group_index: 1,
                },
            ]
        );
    }

    #[test]
    fn self_issuance_without_carried_assets_has_only_issued_group() {
        let server_info = test_server_info();

        let (input, own_address) = self_issuance_input(Vec::new());

        let res = build_self_asset_issuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            123,
            None,
            None,
        )
        .unwrap();

        // Newly minted asset.
        let issued_group = AssetGroup {
            asset_id: None,
            // Cannot be reissued.
            control_asset: None,
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 123,
            }],
        };

        let expected_packet = Packet {
            groups: vec![issued_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_issuance_preserves_carried_assets_on_output_zero() {
        let server_info = test_server_info();
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([11; 32]),
            group_index: 4,
        };

        let (input, own_address) = self_issuance_input(vec![Asset {
            asset_id: unrelated_asset_id,
            amount: 7,
        }]);

        let res = build_self_asset_issuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            123,
            None,
            None,
        )
        .unwrap();

        // Newly minted asset.
        let issued_group = AssetGroup {
            asset_id: None,
            // Cannot be reissued.
            control_asset: None,
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 123,
            }],
        };

        // Get back unrelated asset in full.
        let unrelated_group = AssetGroup {
            asset_id: Some(unrelated_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 7,
            }],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 7,
            }],
        };

        let expected_packet = Packet {
            groups: vec![issued_group, unrelated_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_issuance_with_new_control_asset_emits_control_group_before_issued_group() {
        let server_info = test_server_info();
        let (input, own_address) = self_issuance_input(Vec::new());

        let res = build_self_asset_issuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            123,
            // Configured to mint a new control asset.
            Some(ControlAssetConfig::new(5).unwrap()),
            None,
        )
        .unwrap();

        // Acquire control asset in full.
        let control_group = AssetGroup {
            asset_id: None,
            control_asset: None,
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 5,
            }],
        };

        // Newly minted asset.
        let issued_group = AssetGroup {
            asset_id: None,
            // Can be reissued. Referenced control asset by group because it's in the same
            // transaction.
            control_asset: Some(asset::packet::AssetRef::ByGroup(0)),
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 123,
            }],
        };

        let expected_packet = Packet {
            groups: vec![control_group, issued_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_issuance_with_existing_control_asset_preserves_it_and_references_it_by_id() {
        let server_info = test_server_info();
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([14; 32]),
            group_index: 2,
        };

        let (input, own_address) = self_issuance_input(vec![
            // Control asset.
            Asset {
                asset_id: control_asset_id,
                amount: 3,
            },
        ]);

        let res = build_self_asset_issuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            123,
            // Configured to reuse an existing control asset.
            Some(ControlAssetConfig::existing(control_asset_id)),
            None,
        )
        .unwrap();

        // Newly minted asset.
        let issued_group = AssetGroup {
            asset_id: None,
            // Can be reissued. Referenced control asset by ID because it existed before this
            // transaction.
            control_asset: Some(asset::packet::AssetRef::ById(control_asset_id)),
            metadata: None,
            inputs: vec![],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 123,
            }],
        };

        // Get back control asset in full.
        let control_group = AssetGroup {
            asset_id: Some(control_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 3,
            }],
            outputs: vec![AssetOutput {
                output_index: SELF_ISSUANCE_OUTPUT_INDEX,
                amount: 3,
            }],
        };

        let expected_packet = Packet {
            groups: vec![issued_group, control_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_issuance_without_control_asset_input_errors() {
        let server_info = test_server_info();
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([16; 32]),
            group_index: 3,
        };
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([17; 32]),
            group_index: 1,
        };
        let (input, own_address) = self_issuance_input(vec![Asset {
            asset_id: unrelated_asset_id,
            amount: 9,
        }]);

        let err = build_self_asset_issuance_transactions(
            &own_address,
            &own_address,
            &[input],
            &server_info,
            123,
            Some(ControlAssetConfig::existing(control_asset_id)),
            None,
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("control asset missing from issuance transaction inputs"));
    }

    fn test_server_info() -> server::Info {
        let signer_pk = "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0"
            .parse()
            .unwrap();
        let forfeit_pk = "03dff1d77f2a671c5f36183726db2341be58f8be17d2a3d1d2cd47b7b0f5f2d624"
            .parse()
            .unwrap();

        server::Info {
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
            max_tx_weight: 40_000,
            max_op_return_outputs: 3,
        }
    }

    fn self_issuance_input(assets: Vec<Asset>) -> (VtxoInput, ArkAddress) {
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
            VtxoInput::new(
                spend_script.clone(),
                None,
                control_block,
                vec![spend_script],
                own_address.to_p2tr_script_pubkey(),
                Amount::from_sat(330),
                OutPoint::new(Txid::from_byte_array([0; 32]), 0),
                assets,
            ),
            own_address,
        )
    }

    // The location of the asset packet in the transaction. It's always the second-to-last output,
    // just before the anchor output.
    fn asset_packet_index(ark_tx: &Psbt) -> usize {
        ark_tx.unsigned_tx.output.len() - 2
    }
}
