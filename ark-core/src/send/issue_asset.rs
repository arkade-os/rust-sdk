use super::build_offchain_transactions;
use super::OffchainTransactions;
use super::VtxoInput;
use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::asset::ControlAssetConfig;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use bitcoin::Psbt;
use bitcoin::Txid;
use std::collections::HashMap;

/// A VTXO input together with any assets it currently carries.
#[derive(Debug, Clone)]
pub struct AssetBearingVtxoInput {
    pub input: VtxoInput,
    pub assets: Vec<server::Asset>,
}

/// Unsigned transactions for self asset issuance plus the derived asset IDs.
#[derive(Debug, Clone)]
pub struct SelfAssetIssuanceTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
    pub asset_ids: Vec<AssetId>,
}

/// Build unsigned offchain transactions for issuing a fresh asset to self.
///
/// The issued asset is always placed on output `0`, with `server_info.dust` used as the
/// carrier amount for the self-issued VTXO.
///
/// Assets already carried by the selected inputs are preserved on the BTC change output when one
/// exists. Otherwise, they are preserved on the self-issued output at index `0`.
///
/// This builder is therefore intended for self-issuance flows, where output `0` remains under the
/// issuer's control.
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
pub fn build_self_asset_issuance_transactions(
    own_address: &ArkAddress,
    change_address: &ArkAddress,
    inputs: &[AssetBearingVtxoInput],
    server_info: &server::Info,
    amount: u64,
    control_asset_config: Option<ControlAssetConfig>,
    metadata: Option<Vec<(String, String)>>,
) -> Result<SelfAssetIssuanceTransactions, Error> {
    if amount == 0 {
        return Err(Error::ad_hoc("asset amount must be > 0"));
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

    let mut groups = Vec::new();

    // If creating a new control asset, it goes first.
    if let Some(ControlAssetConfig::New { amount: ctrl_amt }) = &control_asset_config {
        groups.push(asset::packet::AssetGroup {
            asset_id: None,
            control_asset: None,
            metadata: metadata.clone(),
            inputs: vec![],
            outputs: vec![asset::packet::AssetOutput {
                output_index: 0,
                amount: (*ctrl_amt).into(),
            }],
        });
    }

    // Include issued asset.
    {
        let control_asset_ref = match &control_asset_config {
            Some(ControlAssetConfig::New { .. }) => Some(asset::packet::AssetRef::ByGroup(0)),
            Some(ControlAssetConfig::Existing { id }) => Some(asset::packet::AssetRef::ById(*id)),
            None => None,
        };

        groups.push(asset::packet::AssetGroup {
            asset_id: None,
            control_asset: control_asset_ref,
            metadata,
            inputs: vec![],
            outputs: vec![asset::packet::AssetOutput {
                output_index: 0,
                amount,
            }],
        });
    }

    // Preserve any assets carried by the funding VTXOs.
    let existing_asset_output_index = preserved_asset_output_index(&ark_tx);
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
                        output_index: existing_asset_output_index,
                        amount: 0,
                    }],
                });

            group.inputs.push(asset::packet::AssetInput {
                input_index: input_index as u16,
                amount: asset.amount,
            });

            group.outputs[0].amount += asset.amount;
        }
    }

    groups.extend(existing_asset_groups.into_values());

    let packet = asset::packet::Packet { groups };
    add_asset_packet_to_psbt(&mut ark_tx, &packet);

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

/// Return the output index used to preserve assets already carried by the selected inputs.
///
/// Before the asset packet is inserted, `build_offchain_transactions` produces
/// `[receiver, optional change, anchor]`. Existing carried assets stay on the BTC change output
/// when it exists; otherwise, for self-issuance, they fall back to the self-issued output at
/// index `0`.
fn preserved_asset_output_index(ark_tx: &Psbt) -> u16 {
    let num_psbt_outputs = ark_tx.unsigned_tx.output.len();
    let has_change_output = num_psbt_outputs > 2; // receiver + optional change + anchor

    if has_change_output {
        (num_psbt_outputs - 2) as u16
    } else {
        0
    }
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

        // Exact dust input: enough to create the issuance output, but no BTC change output.
        let (input, own_address) = self_issuance_input(10, 330, vec![]);

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

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 3);

        let expected_packet = Packet {
            groups: vec![AssetGroup {
                asset_id: None,
                control_asset: None,
                metadata: None,
                inputs: vec![],
                outputs: vec![AssetOutput {
                    output_index: 0,
                    amount: 123,
                }],
            }],
        };

        assert_eq!(res.ark_tx.unsigned_tx.output[1], expected_packet.to_txout());
    }

    #[test]
    fn self_issuance_with_carried_assets_preserves_them_on_change_output_when_present() {
        let server_info = test_server_info();
        let existing_asset_id = AssetId {
            txid: Txid::from_byte_array([11; 32]),
            group_index: 4,
        };

        // 2x dust input: one dust output for issuance plus one dust BTC change output.
        let (input, own_address) = self_issuance_input(
            12,
            660,
            vec![Asset {
                asset_id: existing_asset_id,
                amount: 7,
            }],
        );

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

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 4);

        let expected_packet = Packet {
            groups: vec![
                AssetGroup {
                    asset_id: None,
                    control_asset: None,
                    metadata: None,
                    inputs: vec![],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 123,
                    }],
                },
                AssetGroup {
                    asset_id: Some(existing_asset_id),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![AssetInput {
                        input_index: 0,
                        amount: 7,
                    }],
                    outputs: vec![AssetOutput {
                        output_index: 1,
                        amount: 7,
                    }],
                },
            ],
        };

        assert_eq!(res.ark_tx.unsigned_tx.output[2], expected_packet.to_txout());
    }

    #[test]
    fn self_issuance_with_carried_assets_and_no_btc_change_preserves_them_on_output_zero() {
        let server_info = test_server_info();
        let existing_asset_id = AssetId {
            txid: Txid::from_byte_array([13; 32]),
            group_index: 2,
        };

        // Exact dust input again: carried assets must fall back to the self-issuance output.
        let (input, own_address) = self_issuance_input(
            14,
            330,
            vec![Asset {
                asset_id: existing_asset_id,
                amount: 11,
            }],
        );

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

        assert_eq!(res.ark_tx.unsigned_tx.output.len(), 3);

        let expected_packet = Packet {
            groups: vec![
                AssetGroup {
                    asset_id: None,
                    control_asset: None,
                    metadata: None,
                    inputs: vec![],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 123,
                    }],
                },
                AssetGroup {
                    asset_id: Some(existing_asset_id),
                    control_asset: None,
                    metadata: None,
                    inputs: vec![AssetInput {
                        input_index: 0,
                        amount: 11,
                    }],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 11,
                    }],
                },
            ],
        };

        assert_eq!(res.ark_tx.unsigned_tx.output[1], expected_packet.to_txout());
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

    fn self_issuance_input(
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
