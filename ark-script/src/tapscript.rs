//! Tapscript encoders used by arkade VTXO leaves.
//!
//! Only the shapes consumed by the arkade flow are implemented:
//!
//! - [`ArkadeTapscript::Multisig`] — n-of-n checksig chain (`<pk1> CHECKSIGVERIFY ... <pkN>
//!   CHECKSIG`).
//! - [`ArkadeTapscript::CsvMultisig`] — same, prefixed with a relative timelock (`<seq>
//!   CHECKSEQUENCEVERIFY DROP ...`).
//!
//! Other shapes (`CLTVMultisig`, `ConditionMultisig`, `ConditionCSVMultisig`
//! in the TS SDK) land when a downstream layer needs them.

use bitcoin::opcodes::all::OP_CHECKSIG;
use bitcoin::opcodes::all::OP_CHECKSIGVERIFY;
use bitcoin::opcodes::all::OP_CSV;
use bitcoin::opcodes::all::OP_DROP;
use bitcoin::script::Builder;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;

/// Errors produced when encoding an [`ArkadeTapscript`].
#[derive(Debug, thiserror::Error)]
pub enum TapscriptError {
    #[error("tapscript requires at least one pubkey")]
    NoPubkeys,
}

/// Subset of tapscript leaf shapes used by arkade flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArkadeTapscript {
    /// `<pk1> CHECKSIGVERIFY <pk2> CHECKSIGVERIFY ... <pkN> CHECKSIG`
    Multisig { pubkeys: Vec<XOnlyPublicKey> },
    /// `<seq> CHECKSEQUENCEVERIFY DROP <pk1> CHECKSIGVERIFY ... <pkN> CHECKSIG`
    CsvMultisig {
        timelock: Sequence,
        pubkeys: Vec<XOnlyPublicKey>,
    },
}

impl ArkadeTapscript {
    /// Serialise this tapscript to its canonical byte encoding.
    pub fn encode(&self) -> Result<ScriptBuf, TapscriptError> {
        match self {
            Self::Multisig { pubkeys } => encode_multisig(pubkeys),
            Self::CsvMultisig { timelock, pubkeys } => encode_csv_multisig(*timelock, pubkeys),
        }
    }

    /// Return a new tapscript with `extra` keys appended to its pubkey set.
    ///
    /// Used by the arkade flow to tack tweaked introspector keys onto an
    /// existing multisig leaf.
    pub fn with_additional_pubkeys(&self, extra: impl IntoIterator<Item = XOnlyPublicKey>) -> Self {
        match self {
            Self::Multisig { pubkeys } => {
                let mut pubkeys = pubkeys.clone();
                pubkeys.extend(extra);
                Self::Multisig { pubkeys }
            }
            Self::CsvMultisig { timelock, pubkeys } => {
                let mut pubkeys = pubkeys.clone();
                pubkeys.extend(extra);
                Self::CsvMultisig {
                    timelock: *timelock,
                    pubkeys,
                }
            }
        }
    }

    /// All signer pubkeys in this tapscript, in order.
    pub fn pubkeys(&self) -> &[XOnlyPublicKey] {
        match self {
            Self::Multisig { pubkeys } | Self::CsvMultisig { pubkeys, .. } => pubkeys,
        }
    }
}

fn encode_multisig(pubkeys: &[XOnlyPublicKey]) -> Result<ScriptBuf, TapscriptError> {
    if pubkeys.is_empty() {
        return Err(TapscriptError::NoPubkeys);
    }

    let mut builder = Builder::new();
    let last = pubkeys.len() - 1;
    for (i, pk) in pubkeys.iter().enumerate() {
        builder = builder.push_x_only_key(pk);
        builder = if i == last {
            builder.push_opcode(OP_CHECKSIG)
        } else {
            builder.push_opcode(OP_CHECKSIGVERIFY)
        };
    }
    Ok(builder.into_script())
}

fn encode_csv_multisig(
    timelock: Sequence,
    pubkeys: &[XOnlyPublicKey],
) -> Result<ScriptBuf, TapscriptError> {
    let multisig = encode_multisig(pubkeys)?;

    let prefix = Builder::new()
        .push_int(timelock.to_consensus_u32() as i64)
        .push_opcode(OP_CSV)
        .push_opcode(OP_DROP)
        .into_script();

    let mut bytes = prefix.into_bytes();
    bytes.extend_from_slice(multisig.as_bytes());
    Ok(ScriptBuf::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(byte: u8) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&[byte; 32]).unwrap()
    }

    // Vectors from ts-sdk @ arkade-script-final:
    // `MultisigTapscript.encode({ pubkeys }).script` and
    // `CSVMultisigTapscript.encode({ timelock, pubkeys }).script`.
    const MULTISIG_CHECKSIG_2: &str = concat!(
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ad",
        "20",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "ac",
    );
    const MULTISIG_CHECKSIG_3: &str = concat!(
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ad",
        "20",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "ad",
        "20",
        "0707070707070707070707070707070707070707070707070707070707070707",
        "ac",
    );
    const CSV_BLOCKS_5120: &str = concat!(
        "020014",
        "b2",
        "75",
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ad",
        "20",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "ac",
    );
    const CSV_SECONDS_512: &str = concat!(
        "03010040",
        "b2",
        "75",
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ad",
        "20",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "ac",
    );
    const CSV_BLOCKS_10_SINGLE: &str = concat!(
        "5a",
        "b2",
        "75",
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ac",
    );

    #[test]
    fn multisig_two_pubkeys_matches_ts() {
        let script = ArkadeTapscript::Multisig {
            pubkeys: vec![pk(0x01), pk(0x02)],
        }
        .encode()
        .unwrap();
        assert_eq!(hex::encode(script.as_bytes()), MULTISIG_CHECKSIG_2);
    }

    #[test]
    fn multisig_three_pubkeys_matches_ts() {
        let script = ArkadeTapscript::Multisig {
            pubkeys: vec![pk(0x01), pk(0x02), pk(0x07)],
        }
        .encode()
        .unwrap();
        assert_eq!(hex::encode(script.as_bytes()), MULTISIG_CHECKSIG_3);
    }

    #[test]
    fn multisig_single_pubkey_uses_checksig_only() {
        let script = ArkadeTapscript::Multisig {
            pubkeys: vec![pk(0x01)],
        }
        .encode()
        .unwrap();
        assert_eq!(
            hex::encode(script.as_bytes()),
            concat!(
                "20",
                "0101010101010101010101010101010101010101010101010101010101010101",
                "ac"
            )
        );
    }

    #[test]
    fn multisig_empty_pubkeys_errors() {
        let err = ArkadeTapscript::Multisig { pubkeys: vec![] }
            .encode()
            .unwrap_err();
        assert!(matches!(err, TapscriptError::NoPubkeys));
    }

    #[test]
    fn csv_blocks_matches_ts() {
        let script = ArkadeTapscript::CsvMultisig {
            timelock: Sequence::from_height(5120),
            pubkeys: vec![pk(0x01), pk(0x02)],
        }
        .encode()
        .unwrap();
        assert_eq!(hex::encode(script.as_bytes()), CSV_BLOCKS_5120);
    }

    #[test]
    fn csv_seconds_matches_ts() {
        let script = ArkadeTapscript::CsvMultisig {
            timelock: Sequence::from_seconds_floor(512).unwrap(),
            pubkeys: vec![pk(0x01), pk(0x02)],
        }
        .encode()
        .unwrap();
        assert_eq!(hex::encode(script.as_bytes()), CSV_SECONDS_512);
    }

    #[test]
    fn csv_small_block_count_uses_pushnum_matches_ts() {
        let script = ArkadeTapscript::CsvMultisig {
            timelock: Sequence::from_height(10),
            pubkeys: vec![pk(0x01)],
        }
        .encode()
        .unwrap();
        assert_eq!(hex::encode(script.as_bytes()), CSV_BLOCKS_10_SINGLE);
    }

    #[test]
    fn with_additional_pubkeys_appends() {
        let base = ArkadeTapscript::Multisig {
            pubkeys: vec![pk(0x01), pk(0x02)],
        };
        let augmented = base.with_additional_pubkeys([pk(0x07)]);
        let pubkeys = augmented.pubkeys();
        assert_eq!(pubkeys.len(), 3);
        assert_eq!(pubkeys[2], pk(0x07));
    }

    #[test]
    fn with_additional_pubkeys_preserves_timelock() {
        let base = ArkadeTapscript::CsvMultisig {
            timelock: Sequence::from_height(100),
            pubkeys: vec![pk(0x01)],
        };
        let augmented = base.with_additional_pubkeys([pk(0x02)]);
        match augmented {
            ArkadeTapscript::CsvMultisig { timelock, pubkeys } => {
                assert_eq!(timelock, Sequence::from_height(100));
                assert_eq!(pubkeys.len(), 2);
            }
            _ => panic!("expected CsvMultisig"),
        }
    }
}
