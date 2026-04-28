//! Arkade extension opcode constants and name lookup.
//!
//! Arkade opcodes occupy byte values `0xb3` (repurposed NOP4) and `0xc4..=0xf5`,
//! which overlap with Bitcoin's `OP_NOP4` and `OP_RETURN_*` opcodes. When these
//! bytes appear in an arkade context they carry the extension semantics instead.

use bitcoin::Opcode;

/// Arkade extension opcodes, aliased to the matching `bitcoin` crate opcode
/// by byte value (arkade repurposes `OP_NOP4` and `OP_RETURN_196..=OP_RETURN_245`).
///
/// Use these with [`bitcoin::script::Builder::push_opcode`] to build
/// arkade-aware scripts.
pub mod op {
    use bitcoin::opcodes::all::*;
    use bitcoin::Opcode;

    // Merkle Branch Verification (0xb3 — repurposed NOP4 slot).
    pub const MERKLEBRANCHVERIFY: Opcode = OP_NOP4;

    // SHA256 streaming (0xc4..=0xc6).
    pub const SHA256INITIALIZE: Opcode = OP_RETURN_196;
    pub const SHA256UPDATE: Opcode = OP_RETURN_197;
    pub const SHA256FINALIZE: Opcode = OP_RETURN_198;

    // Input introspection (0xc7..=0xcb).
    pub const INSPECTINPUTOUTPOINT: Opcode = OP_RETURN_199;
    pub const INSPECTINPUTARKADESCRIPTHASH: Opcode = OP_RETURN_200;
    pub const INSPECTINPUTVALUE: Opcode = OP_RETURN_201;
    pub const INSPECTINPUTSCRIPTPUBKEY: Opcode = OP_RETURN_202;
    pub const INSPECTINPUTSEQUENCE: Opcode = OP_RETURN_203;

    // Signatures (0xcc..=0xcd).
    pub const CHECKSIGFROMSTACK: Opcode = OP_RETURN_204;
    pub const PUSHCURRENTINPUTINDEX: Opcode = OP_RETURN_205;

    // Input arkade witness introspection (0xce).
    pub const INSPECTINPUTARKADEWITNESSHASH: Opcode = OP_RETURN_206;

    // Output introspection (0xcf, 0xd1).
    pub const INSPECTOUTPUTVALUE: Opcode = OP_RETURN_207;
    pub const INSPECTOUTPUTSCRIPTPUBKEY: Opcode = OP_RETURN_209;

    // Transaction introspection (0xd2..=0xd6).
    pub const INSPECTVERSION: Opcode = OP_RETURN_210;
    pub const INSPECTLOCKTIME: Opcode = OP_RETURN_211;
    pub const INSPECTNUMINPUTS: Opcode = OP_RETURN_212;
    pub const INSPECTNUMOUTPUTS: Opcode = OP_RETURN_213;
    pub const TXWEIGHT: Opcode = OP_RETURN_214;

    // Conversion (0xd7..=0xd8).
    pub const NUM2BIN: Opcode = OP_RETURN_215;
    pub const BIN2NUM: Opcode = OP_RETURN_216;

    // EC operations (0xe3..=0xe4).
    pub const ECMULSCALARVERIFY: Opcode = OP_RETURN_227;
    pub const TWEAKVERIFY: Opcode = OP_RETURN_228;

    // Asset groups (0xe5..=0xf2).
    pub const INSPECTNUMASSETGROUPS: Opcode = OP_RETURN_229;
    pub const INSPECTASSETGROUPASSETID: Opcode = OP_RETURN_230;
    pub const INSPECTASSETGROUPCTRL: Opcode = OP_RETURN_231;
    pub const FINDASSETGROUPBYASSETID: Opcode = OP_RETURN_232;
    pub const INSPECTASSETGROUPMETADATAHASH: Opcode = OP_RETURN_233;
    pub const INSPECTASSETGROUPNUM: Opcode = OP_RETURN_234;
    pub const INSPECTASSETGROUP: Opcode = OP_RETURN_235;
    pub const INSPECTASSETGROUPSUM: Opcode = OP_RETURN_236;
    pub const INSPECTOUTASSETCOUNT: Opcode = OP_RETURN_237;
    pub const INSPECTOUTASSETAT: Opcode = OP_RETURN_238;
    pub const INSPECTOUTASSETLOOKUP: Opcode = OP_RETURN_239;
    pub const INSPECTINASSETCOUNT: Opcode = OP_RETURN_240;
    pub const INSPECTINASSETAT: Opcode = OP_RETURN_241;
    pub const INSPECTINASSETLOOKUP: Opcode = OP_RETURN_242;

    // Transaction ID (0xf3).
    pub const TXID: Opcode = OP_RETURN_243;

    // Packet introspection (0xf4..=0xf5).
    pub const INSPECTPACKET: Opcode = OP_RETURN_244;
    pub const INSPECTINPUTPACKET: Opcode = OP_RETURN_245;
}

/// Arkade extension opcode byte → arkade-specific name (no `OP_` prefix).
///
/// When an opcode byte is also a standard Bitcoin opcode (e.g. `OP_NOP4`
/// shares `0xb3` with `MERKLEBRANCHVERIFY`), the arkade name wins in
/// arkade contexts.
const ARKADE_NAMES: &[(u8, &str)] = &[
    (0xb3, "MERKLEBRANCHVERIFY"),
    (0xc4, "SHA256INITIALIZE"),
    (0xc5, "SHA256UPDATE"),
    (0xc6, "SHA256FINALIZE"),
    (0xc7, "INSPECTINPUTOUTPOINT"),
    (0xc8, "INSPECTINPUTARKADESCRIPTHASH"),
    (0xc9, "INSPECTINPUTVALUE"),
    (0xca, "INSPECTINPUTSCRIPTPUBKEY"),
    (0xcb, "INSPECTINPUTSEQUENCE"),
    (0xcc, "CHECKSIGFROMSTACK"),
    (0xcd, "PUSHCURRENTINPUTINDEX"),
    (0xce, "INSPECTINPUTARKADEWITNESSHASH"),
    (0xcf, "INSPECTOUTPUTVALUE"),
    (0xd1, "INSPECTOUTPUTSCRIPTPUBKEY"),
    (0xd2, "INSPECTVERSION"),
    (0xd3, "INSPECTLOCKTIME"),
    (0xd4, "INSPECTNUMINPUTS"),
    (0xd5, "INSPECTNUMOUTPUTS"),
    (0xd6, "TXWEIGHT"),
    (0xd7, "NUM2BIN"),
    (0xd8, "BIN2NUM"),
    (0xe3, "ECMULSCALARVERIFY"),
    (0xe4, "TWEAKVERIFY"),
    (0xe5, "INSPECTNUMASSETGROUPS"),
    (0xe6, "INSPECTASSETGROUPASSETID"),
    (0xe7, "INSPECTASSETGROUPCTRL"),
    (0xe8, "FINDASSETGROUPBYASSETID"),
    (0xe9, "INSPECTASSETGROUPMETADATAHASH"),
    (0xea, "INSPECTASSETGROUPNUM"),
    (0xeb, "INSPECTASSETGROUP"),
    (0xec, "INSPECTASSETGROUPSUM"),
    (0xed, "INSPECTOUTASSETCOUNT"),
    (0xee, "INSPECTOUTASSETAT"),
    (0xef, "INSPECTOUTASSETLOOKUP"),
    (0xf0, "INSPECTINASSETCOUNT"),
    (0xf1, "INSPECTINASSETAT"),
    (0xf2, "INSPECTINASSETLOOKUP"),
    (0xf3, "TXID"),
    (0xf4, "INSPECTPACKET"),
    (0xf5, "INSPECTINPUTPACKET"),
];

/// All arkade extension opcode byte values.
pub const ARKADE_OPCODES: &[u8] = &[
    0xb3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0xd1, 0xd2, 0xd3,
    0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed,
    0xee, 0xef, 0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5,
];

/// Returns the opcode name (with `OP_` prefix) for the given byte, preferring
/// arkade-specific names over Bitcoin names where they collide.
///
/// Data push opcodes `0x01..=0x4b` are rendered as `OP_DATA_N`.
pub fn opcode_name(byte: u8) -> Option<String> {
    if (0x01..=0x4b).contains(&byte) {
        return Some(format!("OP_DATA_{byte}"));
    }

    if let Some((_, name)) = ARKADE_NAMES.iter().find(|(b, _)| *b == byte) {
        return Some(format!("OP_{name}"));
    }

    bitcoin_opcode_name(byte)
}

/// Returns the opcode byte for the given name. Accepts names with or without
/// the `OP_` prefix, and recognises the `OP_DATA_N` pattern used for data
/// push opcodes.
pub fn opcode_from_name(name: &str) -> Option<u8> {
    let stripped = name.strip_prefix("OP_").unwrap_or(name);

    if let Some(rest) = stripped.strip_prefix("DATA_") {
        let n: u16 = rest.parse().ok()?;
        if (1..=0x4b).contains(&n) {
            return Some(n as u8);
        }
        return None;
    }

    if let Some((byte, _)) = ARKADE_NAMES.iter().find(|(_, n)| *n == stripped) {
        return Some(*byte);
    }

    bitcoin_opcode_byte(stripped)
}

fn bitcoin_opcode_name(byte: u8) -> Option<String> {
    // Reuse the bitcoin crate's Display impl, which renders as `OP_NAME`.
    let op = Opcode::from(byte);
    let name = format!("{op}");
    // The bitcoin crate prints unknown opcodes as a hex byte rather than a
    // name; filter those out so callers can detect unknown bytes.
    if name.starts_with("OP_") {
        Some(name)
    } else {
        None
    }
}

fn bitcoin_opcode_byte(stripped: &str) -> Option<u8> {
    // There is no public name → opcode lookup in the `bitcoin` crate, so we
    // round-trip via Display: try every byte until one prints to the target
    // name. The table is 256 entries so this is cheap and avoids maintaining
    // a second copy of the opcode list in sync with the upstream crate.
    //
    // Also handle a few aliases the Display impl doesn't emit.
    match stripped {
        "0" | "FALSE" => return Some(0x00),
        "TRUE" => return Some(0x51),
        "1NEGATE" => return Some(0x4f),
        _ => {}
    }

    // `OP_1`..`OP_16` alias `OP_PUSHNUM_1`..`OP_PUSHNUM_16`.
    if let Some(rest) = stripped.strip_prefix("PUSHNUM_") {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=16).contains(&n) {
                return Some(0x50 + n);
            }
        }
    }
    if let Ok(n) = stripped.parse::<u8>() {
        if (1..=16).contains(&n) {
            return Some(0x50 + n);
        }
    }

    for byte in 0u8..=255 {
        let name = format!("{}", Opcode::from(byte));
        if let Some(n) = name.strip_prefix("OP_") {
            if n == stripped {
                return Some(byte);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arkade_opcode_values() {
        assert_eq!(op::MERKLEBRANCHVERIFY.to_u8(), 0xb3);
        assert_eq!(op::SHA256INITIALIZE.to_u8(), 0xc4);
        assert_eq!(op::NUM2BIN.to_u8(), 0xd7);
        assert_eq!(op::TWEAKVERIFY.to_u8(), 0xe4);
        assert_eq!(op::INSPECTINASSETLOOKUP.to_u8(), 0xf2);
        assert_eq!(op::TXID.to_u8(), 0xf3);
        assert_eq!(op::INSPECTPACKET.to_u8(), 0xf4);
        assert_eq!(op::INSPECTINPUTPACKET.to_u8(), 0xf5);
    }

    #[test]
    fn opcode_name_prefers_arkade() {
        // 0xb3 is NOP4 in standard Bitcoin — arkade wins.
        assert_eq!(opcode_name(0xb3).as_deref(), Some("OP_MERKLEBRANCHVERIFY"));
        // 0xc4 is OP_RETURN_196 in standard Bitcoin — arkade wins.
        assert_eq!(opcode_name(0xc4).as_deref(), Some("OP_SHA256INITIALIZE"));
    }

    #[test]
    fn opcode_name_standard_bitcoin() {
        assert_eq!(opcode_name(0x76).as_deref(), Some("OP_DUP"));
        assert_eq!(opcode_name(0xa9).as_deref(), Some("OP_HASH160"));
        assert_eq!(opcode_name(0x88).as_deref(), Some("OP_EQUALVERIFY"));
    }

    #[test]
    fn opcode_name_data_push() {
        assert_eq!(opcode_name(0x14).as_deref(), Some("OP_DATA_20"));
        assert_eq!(opcode_name(0x01).as_deref(), Some("OP_DATA_1"));
        assert_eq!(opcode_name(0x4b).as_deref(), Some("OP_DATA_75"));
    }

    #[test]
    fn opcode_from_name_round_trip() {
        for &(byte, _) in ARKADE_NAMES {
            let name = opcode_name(byte).unwrap();
            assert_eq!(opcode_from_name(&name), Some(byte));
            let without_prefix = name.strip_prefix("OP_").unwrap();
            assert_eq!(opcode_from_name(without_prefix), Some(byte));
        }
    }

    #[test]
    fn opcode_from_name_bitcoin() {
        assert_eq!(opcode_from_name("OP_DUP"), Some(0x76));
        assert_eq!(opcode_from_name("DUP"), Some(0x76));
        assert_eq!(opcode_from_name("OP_CHECKSIG"), Some(0xac));
        assert_eq!(opcode_from_name("OP_0"), Some(0x00));
        assert_eq!(opcode_from_name("OP_FALSE"), Some(0x00));
        assert_eq!(opcode_from_name("OP_TRUE"), Some(0x51));
        assert_eq!(opcode_from_name("OP_1"), Some(0x51));
        assert_eq!(opcode_from_name("OP_16"), Some(0x60));
    }

    #[test]
    fn opcode_from_name_data_push() {
        assert_eq!(opcode_from_name("OP_DATA_20"), Some(0x14));
        assert_eq!(opcode_from_name("OP_DATA_75"), Some(0x4b));
        // Out of range:
        assert_eq!(opcode_from_name("OP_DATA_76"), None);
        assert_eq!(opcode_from_name("OP_DATA_0"), None);
    }

    #[test]
    fn opcode_from_name_unknown() {
        assert_eq!(opcode_from_name("NOTAREALOPCODE"), None);
    }
}
