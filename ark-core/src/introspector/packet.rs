use bitcoin::consensus::encode::Decodable;
use bitcoin::consensus::encode::Encodable;
use bitcoin::consensus::encode::{self};
use bitcoin::io;
use bitcoin::script::PushBytesBuf;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::VarInt;
use bitcoin::Witness;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::io::Read;

const MAGIC_BYTES: [u8; 3] = [0x41, 0x52, 0x4b];
const PACKET_TYPE: u8 = 0x01;
const MAX_ENTRY_COUNT: usize = 1_000;
const MAX_SCRIPT_LENGTH: usize = 10_000;
const MAX_WITNESS_LENGTH: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntrospectorEntry {
    pub vin: u16,
    pub script: ScriptBuf,
    pub witness: Witness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub entries: Vec<IntrospectorEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("empty packet")]
    EmptyPacket,
    #[error("max introspector entry count exceeded, max={max} got={got}")]
    EntryCountExceeded { max: usize, got: usize },
    #[error("empty script at entry {0}")]
    EmptyScript(usize),
    #[error("duplicate vin {vin} at entry {entry}")]
    DuplicateVin { vin: u16, entry: usize },
    #[error("max introspector script length exceeded, max={max} got={got}")]
    ScriptLengthExceeded { max: usize, got: usize },
    #[error("max introspector witness length exceeded, max={max} got={got}")]
    WitnessLengthExceeded { max: usize, got: usize },
    #[error("failed to encode packet: {0}")]
    Encode(io::Error),
    #[error("failed to decode witness: {0}")]
    WitnessDecode(encode::Error),
    #[error("failed to decode packet: {0}")]
    Decode(encode::Error),
    #[error("failed to read packet: {0}")]
    Read(std::io::Error),
    #[error("unexpected {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("failed to build OP_RETURN script: {0}")]
    Script(#[from] bitcoin::script::PushBytesError),
}

impl Packet {
    pub fn new(entries: Vec<IntrospectorEntry>) -> Result<Self, PacketError> {
        let packet = Self { entries };
        packet.validate()?;
        Ok(packet)
    }

    pub fn validate(&self) -> Result<(), PacketError> {
        if self.entries.is_empty() {
            return Err(PacketError::EmptyPacket);
        }

        if self.entries.len() > MAX_ENTRY_COUNT {
            return Err(PacketError::EntryCountExceeded {
                max: MAX_ENTRY_COUNT,
                got: self.entries.len(),
            });
        }

        let mut seen = BTreeSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.script.is_empty() {
                return Err(PacketError::EmptyScript(index));
            }

            let script_len = entry.script.as_bytes().len();
            if script_len > MAX_SCRIPT_LENGTH {
                return Err(PacketError::ScriptLengthExceeded {
                    max: MAX_SCRIPT_LENGTH,
                    got: script_len,
                });
            }

            if !seen.insert(entry.vin) {
                return Err(PacketError::DuplicateVin {
                    vin: entry.vin,
                    entry: index,
                });
            }
        }

        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, PacketError> {
        self.validate()?;

        let mut bytes = Vec::new();
        VarInt(self.entries.len() as u64)
            .consensus_encode(&mut bytes)
            .map_err(PacketError::Encode)?;

        for entry in &self.entries {
            bytes.extend_from_slice(&entry.vin.to_le_bytes());

            let script = entry.script.as_bytes();
            VarInt(script.len() as u64)
                .consensus_encode(&mut bytes)
                .map_err(PacketError::Encode)?;
            bytes.extend_from_slice(script);

            let witness = encode::serialize(&entry.witness);
            if witness.len() > MAX_WITNESS_LENGTH {
                return Err(PacketError::WitnessLengthExceeded {
                    max: MAX_WITNESS_LENGTH,
                    got: witness.len(),
                });
            }
            VarInt(witness.len() as u64)
                .consensus_encode(&mut bytes)
                .map_err(PacketError::Encode)?;
            bytes.extend_from_slice(&witness);
        }

        Ok(bytes)
    }

    pub fn decode(data: &[u8]) -> Result<Self, PacketError> {
        let mut reader = Cursor::new(data);
        let entry_count = VarInt::consensus_decode(&mut reader)
            .map_err(PacketError::Decode)?
            .0 as usize;

        if entry_count > MAX_ENTRY_COUNT {
            return Err(PacketError::EntryCountExceeded {
                max: MAX_ENTRY_COUNT,
                got: entry_count,
            });
        }

        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let mut vin = [0_u8; 2];
            reader.read_exact(&mut vin).map_err(PacketError::Read)?;
            let vin = u16::from_le_bytes(vin);

            let script_len = VarInt::consensus_decode(&mut reader)
                .map_err(PacketError::Decode)?
                .0 as usize;
            if script_len > MAX_SCRIPT_LENGTH {
                return Err(PacketError::ScriptLengthExceeded {
                    max: MAX_SCRIPT_LENGTH,
                    got: script_len,
                });
            }
            let mut script = vec![0_u8; script_len];
            reader.read_exact(&mut script).map_err(PacketError::Read)?;

            let witness_len = VarInt::consensus_decode(&mut reader)
                .map_err(PacketError::Decode)?
                .0 as usize;
            if witness_len > MAX_WITNESS_LENGTH {
                return Err(PacketError::WitnessLengthExceeded {
                    max: MAX_WITNESS_LENGTH,
                    got: witness_len,
                });
            }
            let mut witness_bytes = vec![0_u8; witness_len];
            reader
                .read_exact(&mut witness_bytes)
                .map_err(PacketError::Read)?;
            let mut witness_reader = witness_bytes.as_slice();
            let witness = Witness::consensus_decode(&mut witness_reader)
                .map_err(PacketError::WitnessDecode)?;
            if !witness_reader.is_empty() {
                return Err(PacketError::TrailingBytes(witness_reader.len()));
            }

            entries.push(IntrospectorEntry {
                vin,
                script: ScriptBuf::from_bytes(script),
                witness,
            });
        }

        let remaining = data.len() - reader.position() as usize;
        if remaining != 0 {
            return Err(PacketError::TrailingBytes(remaining));
        }

        Self::new(entries)
    }

    pub fn to_txout(&self) -> Result<TxOut, PacketError> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&MAGIC_BYTES);
        payload.push(PACKET_TYPE);
        let encoded = self.encode()?;
        VarInt(encoded.len() as u64)
            .consensus_encode(&mut payload)
            .map_err(PacketError::Encode)?;
        payload.extend_from_slice(&encoded);

        let push_bytes = PushBytesBuf::try_from(payload)?;
        let script_pubkey = ScriptBuf::builder()
            .push_opcode(bitcoin::opcodes::all::OP_RETURN)
            .push_slice(push_bytes)
            .into_script();

        Ok(TxOut {
            value: Amount::ZERO,
            script_pubkey,
        })
    }
}

pub fn add_packet_to_psbt(psbt: &mut bitcoin::Psbt, packet: &Packet) -> Result<(), PacketError> {
    let txout = packet.to_txout()?;
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

pub fn find_packet(tx: &Transaction) -> Result<Option<Packet>, PacketError> {
    for output in &tx.output {
        let mut instructions = output.script_pubkey.instructions();
        let Some(Ok(bitcoin::script::Instruction::Op(bitcoin::opcodes::all::OP_RETURN))) =
            instructions.next()
        else {
            continue;
        };
        let Some(Ok(bitcoin::script::Instruction::PushBytes(bytes))) = instructions.next() else {
            continue;
        };
        let bytes = bytes.as_bytes();
        if bytes.len() < 4 || bytes[..3] != MAGIC_BYTES || bytes[3] != PACKET_TYPE {
            continue;
        }

        let mut reader = Cursor::new(&bytes[4..]);
        let payload_len = VarInt::consensus_decode(&mut reader)
            .map_err(PacketError::Decode)?
            .0 as usize;
        let offset = 4 + reader.position() as usize;
        if bytes.len() < offset + payload_len {
            return Err(PacketError::TrailingBytes(0));
        }
        return Packet::decode(&bytes[offset..offset + payload_len]).map(Some);
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute;
    use bitcoin::hex::DisplayHex;
    use bitcoin::hex::FromHex;
    use bitcoin::transaction;
    use bitcoin::TxIn;

    fn witness(items: &[&str]) -> Witness {
        Witness::from_slice(
            &items
                .iter()
                .map(|item| Vec::from_hex(item).unwrap())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn matches_go_vectors() {
        let packet = Packet::new(vec![IntrospectorEntry {
            vin: 0,
            script: ScriptBuf::from_bytes(Vec::from_hex("010203").unwrap()),
            witness: witness(&["0405"]),
        }])
        .unwrap();

        assert_eq!(
            packet.encode().unwrap().to_lower_hex_string(),
            "010000030102030401020405"
        );

        let packet = Packet::new(vec![
            IntrospectorEntry {
                vin: 0,
                script: ScriptBuf::from_bytes(Vec::from_hex("01").unwrap()),
                witness: witness(&["02"]),
            },
            IntrospectorEntry {
                vin: 1,
                script: ScriptBuf::from_bytes(Vec::from_hex("0304").unwrap()),
                witness: witness(&["05", "06"]),
            },
            IntrospectorEntry {
                vin: 5,
                script: ScriptBuf::from_bytes(Vec::from_hex("07").unwrap()),
                witness: Witness::default(),
            },
        ])
        .unwrap();

        assert_eq!(
            packet.encode().unwrap().to_lower_hex_string(),
            "0300000101030101020100020304050201050106050001070100"
        );
    }

    #[test]
    fn decode_rejects_invalid_packets() {
        assert!(matches!(Packet::new(vec![]), Err(PacketError::EmptyPacket)));
        assert!(matches!(
            Packet::decode(&Vec::from_hex("0000000101ff").unwrap()),
            Err(PacketError::TrailingBytes(5))
        ));
        assert!(matches!(
            Packet::decode(&Vec::from_hex("010000fd1127").unwrap()),
            Err(PacketError::ScriptLengthExceeded { .. })
        ));
        assert!(matches!(
            Packet::decode(&Vec::from_hex("01000001510200ff").unwrap()),
            Err(PacketError::TrailingBytes(1))
        ));
    }

    #[test]
    fn add_and_find_packet() {
        let packet = Packet::new(vec![IntrospectorEntry {
            vin: 0,
            script: ScriptBuf::from_bytes(Vec::from_hex("51").unwrap()),
            witness: Witness::default(),
        }])
        .unwrap();

        let mut psbt = bitcoin::Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1_000),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        })
        .unwrap();

        add_packet_to_psbt(&mut psbt, &packet).unwrap();
        assert_eq!(psbt.unsigned_tx.output.len(), 3);

        let found = find_packet(&psbt.unsigned_tx).unwrap().unwrap();
        assert_eq!(found, packet);
    }

    #[test]
    fn duplicate_vins_are_rejected() {
        let err = Packet::new(vec![
            IntrospectorEntry {
                vin: 1,
                script: ScriptBuf::from_bytes(vec![0x51]),
                witness: Witness::default(),
            },
            IntrospectorEntry {
                vin: 1,
                script: ScriptBuf::from_bytes(vec![0x52]),
                witness: Witness::default(),
            },
        ])
        .unwrap_err();

        assert!(matches!(
            err,
            PacketError::DuplicateVin { vin: 1, entry: 1 }
        ));
    }
}
