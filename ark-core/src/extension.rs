use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::script::Instruction;
use bitcoin::Amount;
use bitcoin::Script;
use bitcoin::ScriptBuf;
use bitcoin::TxOut;

pub const MAGIC_BYTES: [u8; 3] = [0x41, 0x52, 0x4b];

#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("extension payload length overflows")]
    PayloadLengthOverflow,
    #[error("truncated extension packet type")]
    TruncatedPacketType,
    #[error("truncated extension packet length")]
    TruncatedPacketLength,
    #[error("truncated extension packet payload, expected {expected} bytes got {got}")]
    TruncatedPacketPayload { expected: usize, got: usize },
    #[error("duplicate extension packet type {0}")]
    DuplicatePacketType(u8),
}

pub fn encode_uvarint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn decode_uvarint(data: &[u8], offset: &mut usize) -> Result<u64, ExtensionError> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let Some(byte) = data.get(*offset) else {
            return Err(ExtensionError::TruncatedPacketLength);
        };
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ExtensionError::PayloadLengthOverflow)
}

pub fn is_extension(script: &Script) -> bool {
    extension_payload(script).is_some()
}

pub fn extension_payload(script: &Script) -> Option<&[u8]> {
    let mut instructions = script.instructions();
    if !matches!(instructions.next(), Some(Ok(Instruction::Op(OP_RETURN)))) {
        return None;
    }
    let Some(Ok(Instruction::PushBytes(bytes))) = instructions.next() else {
        return None;
    };
    let bytes = bytes.as_bytes();
    (bytes.len() >= MAGIC_BYTES.len() && bytes[..MAGIC_BYTES.len()] == MAGIC_BYTES).then_some(bytes)
}

pub fn iter_packets(payload: &[u8]) -> Result<Vec<(u8, &[u8])>, ExtensionError> {
    let mut packets = Vec::new();
    let mut offset = MAGIC_BYTES.len();

    while offset < payload.len() {
        let Some(packet_type) = payload.get(offset).copied() else {
            return Err(ExtensionError::TruncatedPacketType);
        };
        offset += 1;

        let packet_len = decode_uvarint(payload, &mut offset)? as usize;
        let end = offset
            .checked_add(packet_len)
            .ok_or(ExtensionError::PayloadLengthOverflow)?;
        if end > payload.len() {
            return Err(ExtensionError::TruncatedPacketPayload {
                expected: end,
                got: payload.len(),
            });
        }

        packets.push((packet_type, &payload[offset..end]));
        offset = end;
    }

    Ok(packets)
}

pub fn find_packet_payload(
    tx: &bitcoin::Transaction,
    packet_type: u8,
) -> Result<Option<&[u8]>, ExtensionError> {
    for output in &tx.output {
        let Some(payload) = extension_payload(&output.script_pubkey) else {
            continue;
        };
        for (current_type, current_payload) in iter_packets(payload)? {
            if current_type == packet_type {
                return Ok(Some(current_payload));
            }
        }
        return Ok(None);
    }

    Ok(None)
}

pub fn packet_txout(packet_type: u8, packet_payload: &[u8]) -> TxOut {
    let mut payload = Vec::new();
    payload.extend_from_slice(&MAGIC_BYTES);
    push_packet(&mut payload, packet_type, packet_payload);

    TxOut {
        value: Amount::ZERO,
        script_pubkey: op_return_script(&payload),
    }
}

pub fn add_packet_to_psbt(
    psbt: &mut bitcoin::Psbt,
    packet_type: u8,
    packet_payload: &[u8],
) -> Result<(), ExtensionError> {
    let mut encoded_packet = Vec::new();
    push_packet(&mut encoded_packet, packet_type, packet_payload);

    for output in &mut psbt.unsigned_tx.output {
        let Some(existing_payload) = extension_payload(&output.script_pubkey) else {
            continue;
        };

        for (existing_type, _) in iter_packets(existing_payload)? {
            if existing_type == packet_type {
                return Err(ExtensionError::DuplicatePacketType(packet_type));
            }
        }

        let mut payload = existing_payload.to_vec();
        payload.extend_from_slice(&encoded_packet);
        output.script_pubkey = op_return_script(&payload);
        return Ok(());
    }

    let txout = packet_txout(packet_type, packet_payload);
    let len = psbt.unsigned_tx.output.len();

    if len == 0 {
        psbt.unsigned_tx.output.push(txout);
        psbt.outputs.push(bitcoin::psbt::Output::default());
        return Ok(());
    }

    let anchor_index = len - 1;
    psbt.unsigned_tx.output.insert(anchor_index, txout);
    psbt.outputs
        .insert(anchor_index, bitcoin::psbt::Output::default());
    Ok(())
}

fn push_packet(payload: &mut Vec<u8>, packet_type: u8, packet_payload: &[u8]) {
    payload.push(packet_type);
    encode_uvarint(payload, packet_payload.len() as u64);
    payload.extend_from_slice(packet_payload);
}

fn op_return_script(data: &[u8]) -> ScriptBuf {
    let mut script = Vec::new();
    script.push(OP_RETURN.to_u8());
    push_data(&mut script, data);
    ScriptBuf::from_bytes(script)
}

fn push_data(script: &mut Vec<u8>, data: &[u8]) {
    let len = data.len();
    if len <= 75 {
        script.push(len as u8);
    } else if len <= 0xff {
        script.push(0x4c);
        script.push(len as u8);
    } else if len <= 0xffff {
        script.push(0x4d);
        script.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        script.push(0x4e);
        script.extend_from_slice(&(len as u32).to_le_bytes());
    }
    script.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute;
    use bitcoin::transaction;
    use bitcoin::TxIn;

    #[test]
    fn encodes_go_uvarint_packet_lengths() {
        let txout = packet_txout(0x01, &[0; 136]);
        let payload = extension_payload(&txout.script_pubkey).unwrap();
        assert_eq!(&payload[..5], &[0x41, 0x52, 0x4b, 0x01, 0x88]);
        assert_eq!(payload[5], 0x01);
    }

    #[test]
    fn appends_to_existing_extension_output() {
        let mut psbt = bitcoin::Psbt::from_unsigned_tx(bitcoin::Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: packet_txout(0x00, &[0xaa]).script_pubkey,
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        })
        .unwrap();

        add_packet_to_psbt(&mut psbt, 0x01, &[0xbb, 0xcc]).unwrap();

        assert_eq!(psbt.unsigned_tx.output.len(), 2);
        let payload = extension_payload(&psbt.unsigned_tx.output[0].script_pubkey).unwrap();
        let packets = iter_packets(payload).unwrap();
        assert_eq!(
            packets,
            vec![(0x00, &[0xaa][..]), (0x01, &[0xbb, 0xcc][..])]
        );
    }
}
