use crate::asset;
use crate::asset::packet::add_asset_packet_to_psbt;
use crate::asset::AssetId;
use crate::send::build_offchain_transactions;
use crate::send::AssetBearingVtxoInput;
use crate::send::OffchainTransactions;
use crate::server;
use crate::ArkAddress;
use crate::Error;
use bitcoin::hashes::Hash as _;
use bitcoin::Psbt;
use std::collections::HashMap;

/// Self-controlled output used by reissuance packet assignments.
const SELF_REISSUANCE_OUTPUT_INDEX: u16 = 0;

/// Unsigned transactions for asset reissuance.
#[derive(Debug, Clone)]
pub struct AssetReissuanceTransactions {
    pub ark_tx: Psbt,
    pub checkpoint_txs: Vec<Psbt>,
}

/// Build unsigned offchain transactions for reissuing an existing asset to self.
///
/// Output [`SELF_REISSUANCE_OUTPUT_INDEX`] remains self-controlled and carries the preserved
/// control-asset balance, the newly reissued asset amount, and any other assets already carried by
/// the selected inputs.
///
/// If the selected inputs already carry units of `reissue_asset_id`, those existing units are
/// merged into the same asset group and output [`SELF_REISSUANCE_OUTPUT_INDEX`] allocation as the
/// newly reissued amount.
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
///
/// # Errors
///
/// Returns an error if `reissue_amount` is zero, or if the selected inputs do not include a
/// non-zero balance of `control_asset_id` to authorize the reissuance.
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

    let packet =
        create_reissuance_packet(inputs, reissue_asset_id, control_asset_id, reissue_amount)?;

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

    add_asset_packet_to_psbt(&mut ark_tx, &packet);

    Ok(AssetReissuanceTransactions {
        ark_tx,
        checkpoint_txs,
    })
}

/// Create the asset packet for a self-reissuance transaction.
///
/// Output [`SELF_REISSUANCE_OUTPUT_INDEX`] is treated as the self-controlled destination for all
/// assets involved in the reissuance flow. The returned control asset, the newly reissued amount,
/// any existing carried balance of `reissue_asset_id`, and any other preserved carried assets are
/// all assigned to output [`SELF_REISSUANCE_OUTPUT_INDEX`].
///
/// # Arguments
///
/// * `inputs` - The selected VTXO inputs to spend, together with any assets they already carry
/// * `reissue_asset_id` - The ID of the existing asset being reissued
/// * `control_asset_id` - The ID of the control asset authorizing the reissuance
/// * `reissue_amount` - The additional amount of the asset to mint
///
/// # Returns
///
/// An [`asset::packet::Packet`] describing how carried and newly reissued assets are assigned to
/// transaction output [`SELF_REISSUANCE_OUTPUT_INDEX`].
///
/// # Errors
///
/// Returns an error if the selected inputs do not include a non-zero balance of
/// `control_asset_id` to authorize the reissuance.
fn create_reissuance_packet(
    inputs: &[AssetBearingVtxoInput],
    reissue_asset_id: AssetId,
    control_asset_id: AssetId,
    reissue_amount: u64,
) -> Result<asset::packet::Packet, Error> {
    struct AssetTransfer {
        inputs: Vec<asset::packet::AssetInput>,
        output_amount: u64,
    }

    let mut transfers: HashMap<AssetId, AssetTransfer> = HashMap::new();

    // For every distinct asset (reissue, control and unrelated!) in the input set, generate asset
    // inputs and collect the total output amount (to ensure asset preservation).
    for (input_index, input) in inputs.iter().enumerate() {
        for asset in &input.assets {
            let transfer = transfers
                .entry(asset.asset_id)
                .or_insert_with(|| AssetTransfer {
                    inputs: Vec::new(),
                    output_amount: 0,
                });

            transfer.inputs.push(asset::packet::AssetInput {
                input_index: input_index as u16,
                amount: asset.amount,
            });
            transfer.output_amount += asset.amount;
        }
    }

    // Ensure that control asset is an input to the transaction. Otherwise, the Arkade server will
    // not authorise the reissuance.
    {
        let control_transfer = transfers.get(&control_asset_id).ok_or_else(|| {
            Error::ad_hoc("control asset missing from reissuance transaction inputs")
        })?;
        let control_input_amount: u64 = control_transfer.inputs.iter().map(|i| i.amount).sum();

        if control_input_amount == 0 {
            return Err(Error::ad_hoc(
                "control asset missing from reissuance transaction inputs",
            ));
        }
    }

    // Actually include the newly minted assets in the asset output for the reissued asset.
    {
        let reissue_transfer = transfers
            .entry(reissue_asset_id)
            .or_insert_with(|| AssetTransfer {
                inputs: Vec::new(),
                output_amount: 0,
            });
        reissue_transfer.output_amount += reissue_amount;
    }

    let mut groups = transfers
        .into_iter()
        .map(|(asset_id, transfer)| asset::packet::AssetGroup {
            asset_id: Some(asset_id),
            control_asset: None,
            metadata: None,
            inputs: transfer.inputs,
            outputs: vec![asset::packet::AssetOutput {
                output_index: SELF_REISSUANCE_OUTPUT_INDEX,
                amount: transfer.output_amount,
            }],
        })
        .collect::<Vec<_>>();

    // Sort the groups to make it easier to test the behaviour. This is _not_ required.
    groups.sort_by_key(|group| {
        let asset_id = group
            .asset_id
            .expect("reissuance packet groups always have asset ids");
        (*asset_id.txid.as_byte_array(), asset_id.group_index)
    });

    Ok(asset::packet::Packet { groups })
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
            Txid::from_byte_array([3; 32]),
            // Only provide control asset as input.
            vec![Asset {
                asset_id: control_asset_id,
                amount: 5,
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

        // Newly minted asset.
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

        // Get back control asset in full.
        let control_group = AssetGroup {
            asset_id: Some(control_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 5,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: 5,
            }],
        };

        let expected_packet = Packet {
            groups: vec![reissued_group, control_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_reissuance_preserves_unrelated_carried_assets_on_output_zero() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([1; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([2; 32]),
            group_index: 1,
        };
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([3; 32]),
            group_index: 2,
        };

        let (input, own_address) = self_reissuance_input(
            Txid::from_byte_array([4; 32]),
            vec![
                // Control asset.
                Asset {
                    asset_id: control_asset_id,
                    amount: 1,
                },
                // Unrelated asset.
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

        // Newly minted asset.
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

        // Get back control asset in full.
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

        // Get back unrelated asset in full.
        let unrelated_group = AssetGroup {
            asset_id: Some(unrelated_asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 9,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: 9,
            }],
        };

        let expected_packet = Packet {
            groups: vec![reissued_group, control_group, unrelated_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_reissuance_with_existing_asset_balance_merges_it_into_output_zero() {
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
            Txid::from_byte_array([3; 32]),
            vec![
                // Control asset.
                Asset {
                    asset_id: control_asset_id,
                    amount: 1,
                },
                // The same asset we are about to reissue.
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

        // Reissued asset includes preserved balance plus newly minted amount.
        let reissued_group = AssetGroup {
            asset_id: Some(asset_id),
            control_asset: None,
            metadata: None,
            inputs: vec![AssetInput {
                input_index: 0,
                amount: 7,
            }],
            outputs: vec![AssetOutput {
                output_index: 0,
                amount: 130,
            }],
        };

        // Get back control asset in full.
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

        let expected_packet = Packet {
            groups: vec![reissued_group, control_group],
        };

        let asset_packet_index = asset_packet_index(&res.ark_tx);

        assert_eq!(
            res.ark_tx.unsigned_tx.output[asset_packet_index],
            expected_packet.to_txout()
        );
    }

    #[test]
    fn self_reissuance_without_control_asset_input_errors() {
        let server_info = test_server_info();
        let asset_id = AssetId {
            txid: Txid::from_byte_array([1; 32]),
            group_index: 0,
        };
        let control_asset_id = AssetId {
            txid: Txid::from_byte_array([2; 32]),
            group_index: 1,
        };
        let unrelated_asset_id = AssetId {
            txid: Txid::from_byte_array([3; 32]),
            group_index: 2,
        };

        let (input, own_address) = self_reissuance_input(
            Txid::from_byte_array([4; 32]),
            vec![Asset {
                asset_id: unrelated_asset_id,
                amount: 9,
            }],
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
            .contains("control asset missing from reissuance transaction inputs"));
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
        asset_bearing_input_txid: Txid,
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
                    Amount::from_sat(330),
                    OutPoint::new(asset_bearing_input_txid, 0),
                ),
                assets,
            },
            own_address,
        )
    }

    // The location of the asset packet in the transaction. It's always the second-to-last output,
    // just before the anchor output.
    fn asset_packet_index(ark_tx: &Psbt) -> usize {
        ark_tx.unsigned_tx.output.len() - 2
    }
}
