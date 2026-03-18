//! Arkade Asset V1 packet encoding.
//!
//! Implements the binary encoding format as specified in the Arkade Asset V1 specification.
//! The packet is embedded in a Bitcoin transaction via an OP_RETURN output.

use bitcoin::ScriptBuf;
use bitcoin::TxOut;
use bitcoin::Txid;

/// Magic bytes prefix: "ARK" (0x41524b).
const MAGIC_BYTES: [u8; 3] = [0x41, 0x52, 0x4b];

/// TLV type byte for the asset packet.
const ASSET_PACKET_TYPE: u8 = 0x00;

/// Presence byte bits for Group optional fields.
const PRESENCE_ASSET_ID: u8 = 0x01;
const PRESENCE_CONTROL_ASSET: u8 = 0x02;
const PRESENCE_METADATA: u8 = 0x04;

/// An asset identifier: (genesis_txid, group_index).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetId {
    pub txid: Txid,
    pub group_index: u16,
}

/// Reference to a control asset.
#[derive(Clone, Debug)]
pub enum AssetRef {
    /// Reference an existing asset by its full ID.
    ById(AssetId),
    /// Reference a group in the same transaction by index.
    ByGroup(u16),
}

/// Key-value metadata map.
pub type Metadata = Vec<(String, String)>;

/// A local asset input referencing a transaction input by index.
#[derive(Clone, Debug)]
pub struct AssetInput {
    /// Index into the transaction's inputs.
    pub input_index: u16,
    /// Amount of asset from this input.
    pub amount: u64,
}

/// A local asset output referencing a transaction output by index.
#[derive(Clone, Debug)]
pub struct AssetOutput {
    /// Index into the transaction's outputs.
    pub output_index: u16,
    /// Amount of asset to this output.
    pub amount: u64,
}

/// A single asset group within a packet.
#[derive(Clone, Debug)]
pub struct AssetGroup {
    /// If `None`, this is a fresh asset issuance. The asset ID will be derived from
    /// `(this_txid, group_index)`.
    pub asset_id: Option<AssetId>,
    /// Control asset reference. Only valid for issuances (when `asset_id` is `None`).
    pub control_asset: Option<AssetRef>,
    /// Metadata key-value pairs attached to the asset group.
    pub metadata: Option<Metadata>,
    /// Asset inputs consumed by this group.
    pub inputs: Vec<AssetInput>,
    /// Asset outputs produced by this group.
    pub outputs: Vec<AssetOutput>,
}

/// A complete asset packet containing one or more asset groups.
#[derive(Clone, Debug)]
pub struct Packet {
    pub groups: Vec<AssetGroup>,
}

impl Packet {
    /// Encode this packet into its binary representation.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_uvarint(&mut buf, self.groups.len() as u64);
        for group in &self.groups {
            group.encode(&mut buf);
        }
        buf
    }

    /// Wrap this packet into an OP_RETURN TxOut with the ARK magic bytes and TLV envelope.
    pub fn to_txout(&self) -> TxOut {
        let payload = self.encode();

        let mut script_data = Vec::new();
        // Magic bytes
        script_data.extend_from_slice(&MAGIC_BYTES);
        // TLV: Type
        script_data.push(ASSET_PACKET_TYPE);
        // TLV: Length (uvarint)
        encode_uvarint(&mut script_data, payload.len() as u64);
        // TLV: Value
        script_data.extend_from_slice(&payload);

        // Build OP_RETURN script
        let mut script_bytes = Vec::new();
        script_bytes.push(0x6a); // OP_RETURN
        push_data(&mut script_bytes, &script_data);

        TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(script_bytes),
        }
    }
}

impl AssetGroup {
    fn encode(&self, buf: &mut Vec<u8>) {
        // Compute presence byte
        let mut presence: u8 = 0;
        if self.asset_id.is_some() {
            presence |= PRESENCE_ASSET_ID;
        }
        if self.control_asset.is_some() {
            presence |= PRESENCE_CONTROL_ASSET;
        }
        if self.metadata.is_some() {
            presence |= PRESENCE_METADATA;
        }
        buf.push(presence);

        // Encode optional fields in order
        if let Some(asset_id) = &self.asset_id {
            asset_id.encode(buf);
        }
        if let Some(control_asset) = &self.control_asset {
            control_asset.encode(buf);
        }
        if let Some(metadata) = &self.metadata {
            encode_metadata(buf, metadata);
        }

        // Encode inputs
        encode_uvarint(buf, self.inputs.len() as u64);
        for input in &self.inputs {
            input.encode(buf);
        }

        // Encode outputs
        encode_uvarint(buf, self.outputs.len() as u64);
        for output in &self.outputs {
            output.encode(buf);
        }
    }
}

impl AssetId {
    fn encode(&self, buf: &mut Vec<u8>) {
        // txid as 32 bytes (internal byte order, not display order)
        buf.extend_from_slice(self.txid.as_ref());
        buf.extend_from_slice(&self.group_index.to_le_bytes());
    }
}

impl AssetRef {
    fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            AssetRef::ById(asset_id) => {
                buf.push(0x01); // BY_ID
                asset_id.encode(buf);
            }
            AssetRef::ByGroup(gidx) => {
                buf.push(0x02); // BY_GROUP
                buf.extend_from_slice(&gidx.to_le_bytes());
            }
        }
    }
}

impl AssetInput {
    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(0x01); // LOCAL
        buf.extend_from_slice(&self.input_index.to_le_bytes());
        encode_uvarint(buf, self.amount);
    }
}

impl AssetOutput {
    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(0x01); // LOCAL
        buf.extend_from_slice(&self.output_index.to_le_bytes());
        encode_uvarint(buf, self.amount);
    }
}

/// Encode a uvarint (LEB128 unsigned variable-length integer).
///
/// This matches Go's `binary.PutUvarint` / protobuf unsigned varint encoding.
fn encode_uvarint(buf: &mut Vec<u8>, mut value: u64) {
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

/// Encode a metadata map: count, then for each entry: key_len + key + value_len + value.
fn encode_metadata(buf: &mut Vec<u8>, metadata: &[(String, String)]) {
    encode_uvarint(buf, metadata.len() as u64);
    for (key, value) in metadata {
        encode_uvarint(buf, key.len() as u64);
        buf.extend_from_slice(key.as_bytes());
        encode_uvarint(buf, value.len() as u64);
        buf.extend_from_slice(value.as_bytes());
    }
}

/// Push data onto a script with proper OP_PUSH prefix.
fn push_data(script: &mut Vec<u8>, data: &[u8]) {
    let len = data.len();
    if len <= 75 {
        script.push(len as u8);
    } else if len <= 0xff {
        script.push(0x4c); // OP_PUSHDATA1
        script.push(len as u8);
    } else if len <= 0xffff {
        script.push(0x4d); // OP_PUSHDATA2
        script.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        script.push(0x4e); // OP_PUSHDATA4
        script.extend_from_slice(&(len as u32).to_le_bytes());
    }
    script.extend_from_slice(data);
}

/// Helper to add an asset packet as an OP_RETURN output to an existing PSBT.
///
/// The P2A (anchor) output must remain the last output. This function inserts
/// the asset packet output before it.
pub fn add_asset_packet_to_psbt(psbt: &mut bitcoin::Psbt, packet: &Packet) {
    if packet.groups.is_empty() {
        return;
    }

    let txout = packet.to_txout();
    let num_outputs = psbt.unsigned_tx.output.len();

    if num_outputs == 0 {
        psbt.unsigned_tx.output.push(txout);
        psbt.outputs.push(bitcoin::psbt::Output::default());
        return;
    }

    // Insert before the last output (P2A anchor).
    let last_idx = num_outputs - 1;
    psbt.unsigned_tx.output.insert(last_idx, txout);
    psbt.outputs
        .insert(last_idx, bitcoin::psbt::Output::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_uvarint() {
        let mut buf = Vec::new();
        encode_uvarint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        encode_uvarint(&mut buf, 127);
        assert_eq!(buf, vec![0x7f]);

        buf.clear();
        encode_uvarint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);

        buf.clear();
        encode_uvarint(&mut buf, 300);
        assert_eq!(buf, vec![0xac, 0x02]);
    }

    #[test]
    fn test_fresh_issuance_no_control() {
        // Fresh asset, no control, 1000 units to output 0
        let packet = Packet {
            groups: vec![AssetGroup {
                asset_id: None,
                control_asset: None,
                metadata: None,
                inputs: vec![],
                outputs: vec![AssetOutput {
                    output_index: 0,
                    amount: 1000,
                }],
            }],
        };

        let encoded = packet.encode();
        // Should start with group count = 1
        assert_eq!(encoded[0], 0x01);
        // Presence byte = 0 (no asset_id, no control_asset, no metadata)
        assert_eq!(encoded[1], 0x00);
        // Input count = 0
        assert_eq!(encoded[2], 0x00);
        // Output count = 1
        assert_eq!(encoded[3], 0x01);
    }

    #[test]
    fn test_fresh_issuance_with_control_by_group() {
        // Control asset group + issued asset group referencing it
        let packet = Packet {
            groups: vec![
                // Group 0: control asset (fresh, no control ref)
                AssetGroup {
                    asset_id: None,
                    control_asset: None,
                    metadata: None,
                    inputs: vec![],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 1,
                    }],
                },
                // Group 1: issued asset referencing group 0 as control
                AssetGroup {
                    asset_id: None,
                    control_asset: Some(AssetRef::ByGroup(0)),
                    metadata: None,
                    inputs: vec![],
                    outputs: vec![AssetOutput {
                        output_index: 0,
                        amount: 1000,
                    }],
                },
            ],
        };

        let encoded = packet.encode();
        // Group count = 2
        assert_eq!(encoded[0], 0x02);
    }

    #[test]
    fn test_to_txout() {
        let packet = Packet {
            groups: vec![AssetGroup {
                asset_id: None,
                control_asset: None,
                metadata: None,
                inputs: vec![],
                outputs: vec![AssetOutput {
                    output_index: 0,
                    amount: 100,
                }],
            }],
        };

        let txout = packet.to_txout();
        assert_eq!(txout.value, bitcoin::Amount::ZERO);

        // Script should start with OP_RETURN (0x6a)
        let script_bytes = txout.script_pubkey.as_bytes();
        assert_eq!(script_bytes[0], 0x6a);

        // After push byte, should have ARK magic
        // push_len byte, then 0x41 0x52 0x4b
        let data_start = 2; // 0x6a + push_len
        assert_eq!(&script_bytes[data_start..data_start + 3], &MAGIC_BYTES);
    }
}
