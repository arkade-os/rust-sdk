//! ASM (assembly) conversion that understands both standard Bitcoin and
//! Arkade extension opcodes.

use super::opcodes::opcode_from_name;
use super::opcodes::opcode_name;
use bitcoin::opcodes::all::OP_PUSHBYTES_0;
use bitcoin::opcodes::all::OP_PUSHNUM_1;
use bitcoin::opcodes::all::OP_PUSHNUM_16;
use bitcoin::script::Builder;
use bitcoin::script::Instruction;
use bitcoin::script::PushBytes;
use bitcoin::Opcode;
use bitcoin::Script;
use bitcoin::ScriptBuf;

/// Errors returned when parsing ASM.
#[derive(Debug, thiserror::Error)]
pub enum AsmError {
    #[error("invalid ASM token: {0}")]
    InvalidToken(String),
    #[error("failed to decode hex token {0:?}: {1}")]
    InvalidHex(String, hex::FromHexError),
    #[error("push payload exceeds maximum size")]
    PushTooLarge,
    #[error("failed to decode script: {0}")]
    ScriptDecode(bitcoin::script::Error),
}

/// Convert a script to ASM format. Standard Bitcoin opcodes and Arkade
/// extension opcodes are rendered by name (with `OP_` prefix); data pushes
/// are rendered as lowercase hex.
///
/// Returns [`AsmError::ScriptDecode`] if the script contains a malformed push.
pub fn to_asm(script: &Script) -> Result<String, AsmError> {
    let mut parts: Vec<String> = Vec::new();

    for instruction in script.instructions() {
        match instruction.map_err(AsmError::ScriptDecode)? {
            Instruction::Op(op) => {
                let name = opcode_name(op.to_u8())
                    .unwrap_or_else(|| format!("OP_UNKNOWN_{:02x}", op.to_u8()));
                parts.push(name);
            }
            Instruction::PushBytes(bytes) => {
                if bytes.is_empty() {
                    parts.push("OP_0".to_string());
                } else {
                    parts.push(hex::encode(bytes.as_bytes()));
                }
            }
        }
    }

    Ok(parts.join(" "))
}

/// Convert raw script bytes to ASM.
pub fn bytes_to_asm(bytes: &[u8]) -> Result<String, AsmError> {
    to_asm(Script::from_bytes(bytes))
}

/// Parse ASM into script bytes.
///
/// Supported tokens:
/// - `OP_NAME` / `NAME` — any standard Bitcoin or Arkade opcode name
/// - `OP_0` / `OP_FALSE` — push the empty byte array
/// - `OP_1`..`OP_16` / `OP_TRUE` — numeric push opcodes
/// - hex strings — pushed as data
pub fn from_asm(asm: &str) -> Result<ScriptBuf, AsmError> {
    let mut builder = Builder::new();

    for token in asm.split_whitespace() {
        builder = append_token(builder, token)?;
    }

    Ok(builder.into_script())
}

fn append_token(builder: Builder, token: &str) -> Result<Builder, AsmError> {
    if token == "OP_0" || token == "OP_FALSE" {
        return Ok(builder.push_opcode(OP_PUSHBYTES_0));
    }
    if token == "OP_TRUE" {
        return Ok(builder.push_opcode(OP_PUSHNUM_1));
    }

    if let Some(rest) = token.strip_prefix("OP_") {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=16).contains(&n) {
                let op = Opcode::from(OP_PUSHNUM_1.to_u8() + n - 1);
                debug_assert!(op.to_u8() <= OP_PUSHNUM_16.to_u8());
                return Ok(builder.push_opcode(op));
            }
        }
    }

    if let Some(byte) = opcode_from_name(token) {
        if (0x01..=0x4b).contains(&byte) {
            // Bare data-push opcodes (e.g. `OP_DATA_20`) aren't meaningful
            // without a payload — reject rather than silently emit a
            // malformed script.
            return Err(AsmError::InvalidToken(token.to_string()));
        }
        return Ok(builder.push_opcode(Opcode::from(byte)));
    }

    if let Some(rest) = token.strip_prefix("OP_UNKNOWN_") {
        if rest.len() != 2 {
            return Err(AsmError::InvalidToken(token.to_string()));
        }
        let byte =
            u8::from_str_radix(rest, 16).map_err(|_| AsmError::InvalidToken(token.to_string()))?;
        if (0x01..=0x4b).contains(&byte) {
            return Err(AsmError::InvalidToken(token.to_string()));
        }
        return Ok(builder.push_opcode(Opcode::from(byte)));
    }

    if is_hex(token) {
        let bytes = hex::decode(token).map_err(|e| AsmError::InvalidHex(token.to_string(), e))?;
        let push: &PushBytes = bytes
            .as_slice()
            .try_into()
            .map_err(|_| AsmError::PushTooLarge)?;
        return Ok(builder.push_slice(push));
    }

    Err(AsmError::InvalidToken(token.to_string()))
}

fn is_hex(token: &str) -> bool {
    !token.is_empty() && token.len() % 2 == 0 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op;
    use bitcoin::opcodes::all::OP_CHECKSIG;
    use bitcoin::opcodes::all::OP_DUP;
    use bitcoin::opcodes::all::OP_EQUALVERIFY;
    use bitcoin::opcodes::all::OP_HASH160;

    #[test]
    fn to_asm_standard_opcodes() {
        let script = Builder::new()
            .push_opcode(OP_DUP)
            .push_opcode(OP_HASH160)
            .into_script();
        assert_eq!(to_asm(&script).unwrap(), "OP_DUP OP_HASH160");
    }

    #[test]
    fn to_asm_arkade_opcodes() {
        let script = Builder::new()
            .push_opcode(op::SHA256INITIALIZE)
            .push_opcode(op::ADD64)
            .push_opcode(op::TWEAKVERIFY)
            .into_script();
        assert_eq!(
            to_asm(&script).unwrap(),
            "OP_SHA256INITIALIZE OP_ADD64 OP_TWEAKVERIFY"
        );
    }

    #[test]
    fn to_asm_mixed() {
        let pubkey_hash = hex::decode("1234567890abcdef1234567890abcdef12345678").unwrap();
        let push: &PushBytes = pubkey_hash.as_slice().try_into().unwrap();
        let script = Builder::new()
            .push_opcode(OP_DUP)
            .push_opcode(OP_HASH160)
            .push_slice(push)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_CHECKSIG)
            .into_script();
        assert_eq!(
            to_asm(&script).unwrap(),
            "OP_DUP OP_HASH160 1234567890abcdef1234567890abcdef12345678 OP_EQUALVERIFY OP_CHECKSIG"
        );
    }

    #[test]
    fn to_asm_merklebranchverify_and_txid() {
        let script = Builder::new()
            .push_opcode(op::MERKLEBRANCHVERIFY)
            .push_opcode(op::TXID)
            .into_script();
        assert_eq!(to_asm(&script).unwrap(), "OP_MERKLEBRANCHVERIFY OP_TXID");
    }

    #[test]
    fn to_asm_empty_push_is_op_0() {
        let script = Builder::new().push_opcode(OP_PUSHBYTES_0).into_script();
        assert_eq!(to_asm(&script).unwrap(), "OP_0");
    }

    #[test]
    fn from_asm_standard_opcodes() {
        let script = from_asm("OP_DUP OP_HASH160").unwrap();
        assert_eq!(
            script,
            Builder::new()
                .push_opcode(OP_DUP)
                .push_opcode(OP_HASH160)
                .into_script()
        );
    }

    #[test]
    fn from_asm_without_op_prefix() {
        let script = from_asm("DUP HASH160").unwrap();
        assert_eq!(
            script,
            Builder::new()
                .push_opcode(OP_DUP)
                .push_opcode(OP_HASH160)
                .into_script()
        );
    }

    #[test]
    fn from_asm_hex_data() {
        let script = from_asm("OP_DUP aabbccdd OP_EQUALVERIFY").unwrap();
        let payload: &PushBytes = [0xaa, 0xbb, 0xcc, 0xdd].as_slice().try_into().unwrap();
        assert_eq!(
            script,
            Builder::new()
                .push_opcode(OP_DUP)
                .push_slice(payload)
                .push_opcode(OP_EQUALVERIFY)
                .into_script()
        );
    }

    #[test]
    fn from_asm_arkade() {
        let script = from_asm("OP_SHA256INITIALIZE OP_ADD64 OP_TWEAKVERIFY").unwrap();
        assert_eq!(
            script,
            Builder::new()
                .push_opcode(op::SHA256INITIALIZE)
                .push_opcode(op::ADD64)
                .push_opcode(op::TWEAKVERIFY)
                .into_script()
        );
    }

    #[test]
    fn from_asm_op_0_and_numeric() {
        let script = from_asm("OP_0 OP_1 OP_16").unwrap();
        assert_eq!(
            script,
            Builder::new()
                .push_opcode(OP_PUSHBYTES_0)
                .push_opcode(OP_PUSHNUM_1)
                .push_opcode(OP_PUSHNUM_16)
                .into_script()
        );
    }

    #[test]
    fn from_asm_invalid_token() {
        let err = from_asm("NOTAREALOPCODE").unwrap_err();
        assert!(matches!(err, AsmError::InvalidToken(_)));
    }

    #[test]
    fn round_trip() {
        let original =
            "OP_DUP OP_HASH160 1234567890abcdef1234567890abcdef12345678 OP_EQUALVERIFY OP_CHECKSIG";
        let script = from_asm(original).unwrap();
        assert_eq!(to_asm(&script).unwrap(), original);
    }

    #[test]
    fn round_trip_arkade() {
        let original = "OP_INSPECTNUMASSETGROUPS OP_ADD64 deadbeef OP_EQUAL";
        let script = from_asm(original).unwrap();
        assert_eq!(to_asm(&script).unwrap(), original);
    }

    #[test]
    fn round_trip_unknown_opcode() {
        let original = ScriptBuf::from_bytes(vec![0xff]);
        let unknown = from_asm("OP_UNKNOWN_ff").unwrap();
        assert_eq!(unknown, original);

        let asm = to_asm(&unknown).unwrap();
        assert_eq!(from_asm(&asm).unwrap(), original);
    }

    #[test]
    fn from_asm_rejects_invalid_unknown_opcode_tokens() {
        for token in [
            "OP_UNKNOWN_f",
            "OP_UNKNOWN_fff",
            "OP_UNKNOWN_gg",
            "OP_UNKNOWN_01",
        ] {
            let err = from_asm(token).unwrap_err();
            assert!(matches!(err, AsmError::InvalidToken(_)));
        }
    }
}
