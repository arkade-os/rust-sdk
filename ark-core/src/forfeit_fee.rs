use crate::Vtxo;
use bitcoin::constants::WITNESS_SCALE_FACTOR;
use bitcoin::Address;
use bitcoin::AddressType;
use bitcoin::Amount;
use bitcoin::FeeRate;
use bitcoin::VarInt;

pub fn compute_forfeit_min_relay_fee(
    fee_rate_sats_per_kvb: u64,
    vtxo: &Vtxo,
    forfeit_address: &Address,
) -> Amount {
    const INPUT_SIZE: u64 = 32 + 4 + 1 + 4;
    const FORFEIT_LEAF_WITNESS_SIZE: u64 = 64 * 2; // 2 signatures for multisig.
    const TAPROOT_BASE_CONTROL_BLOCK_WITNESS_SIZE: u64 = 33;
    const BASE_OUTPUT_SIZE: u64 = 8 + 1;
    const P2PKH_SIZE: u64 = 25;
    const P2SH_SIZE: u64 = 23;
    const P2WKH_SIZE: u64 = 1 + 1 + 20;
    const P2WSH_SIZE: u64 = 1 + 1 + 32;
    const P2TR_SIZE: u64 = 34;
    const P2PKH_OUTPUT_SIZE: u64 = BASE_OUTPUT_SIZE + P2PKH_SIZE;
    const P2SH_OUTPUT_SIZE: u64 = BASE_OUTPUT_SIZE + P2SH_SIZE;
    const P2WKH_OUTPUT_SIZE: u64 = BASE_OUTPUT_SIZE + P2WKH_SIZE;
    const P2WSH_OUTPUT_SIZE: u64 = BASE_OUTPUT_SIZE + P2WSH_SIZE;
    const P2TR_OUTPUT_SIZE: u64 = BASE_OUTPUT_SIZE + P2TR_SIZE;
    const BASE_TX_SIZE: u64 = 4 + 4;
    const TAPROOT_SIGNATURE_WITNESS_SIZE: u64 = 1 + 64;
    const TAPROOT_KEY_PATH_WITNESS_SIZE: u64 = 1 + TAPROOT_SIGNATURE_WITNESS_SIZE;

    let n_inputs = 2;
    let n_outputs = 1;
    let mut input_size = 0;
    let mut witness_size = 0;
    let mut output_size = 0;

    // 1 connector input. We use P2PKH for this!
    input_size += INPUT_SIZE;
    witness_size += TAPROOT_KEY_PATH_WITNESS_SIZE;

    // 1 VTXO input.
    input_size += INPUT_SIZE;

    let spend_info = &vtxo.spend_info();
    let ((biggest_script, leaf_version), _) = spend_info
        .script_map()
        .iter()
        .max_by_key(|((script, leaf_version), _)| {
            let control_block = spend_info
                .control_block(&(script.clone(), *leaf_version))
                .expect("control block");

            control_block.size() + script.len()
        })
        .expect("at least one");

    let control_block = spend_info
        .control_block(&(biggest_script.clone(), *leaf_version))
        .expect("control block");

    // We add 1 byte for the total number of witness elements.
    //
    // 1 byte for the length of the element plus the element itself.
    let control_block_witness_size = 1
        + TAPROOT_BASE_CONTROL_BLOCK_WITNESS_SIZE
        + 1
        + (biggest_script.len() as u64)
        + 1
        + (control_block.merkle_branch.concat().len() as u64);

    witness_size += FORFEIT_LEAF_WITNESS_SIZE + control_block_witness_size;

    match forfeit_address.address_type() {
        Some(AddressType::P2pkh) => {
            output_size += P2PKH_OUTPUT_SIZE;
        }
        Some(AddressType::P2sh) => {
            output_size += P2SH_OUTPUT_SIZE;
        }
        Some(AddressType::P2wpkh) => {
            output_size += P2WKH_OUTPUT_SIZE;
        }
        Some(AddressType::P2wsh) => {
            output_size += P2WSH_OUTPUT_SIZE;
        }
        Some(AddressType::P2tr) => {
            output_size += P2TR_OUTPUT_SIZE;
        }
        _ => unreachable!("Unless they add new witness versions"),
    }

    let input_count = VarInt(n_inputs).size() as u64;
    let output_count = VarInt(n_outputs).size() as u64;
    let tx_size_stripped = BASE_TX_SIZE + input_count + input_size + output_count + output_size;

    let weight_wu = tx_size_stripped * WITNESS_SCALE_FACTOR as u64;
    let weight_wu = weight_wu + witness_size;

    let weight_vb = weight_wu as f64 / 4.0;

    // 1012 sat/kvb == 1012/4 sat/kwu
    let fee_rate = FeeRate::from_sat_per_kwu(fee_rate_sats_per_kvb / 4);

    fee_rate.fee_vb(weight_vb.ceil() as u64).expect("amount")
}
