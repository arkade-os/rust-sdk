//! Processing arkade-enhanced taproot leaves.
//!
//! [`ArkadeVtxoScript::new`] takes a mix of plain taproot leaves and
//! [`ArkadeLeaf`]s; for every arkade leaf it derives a tweaked key per
//! introspector (`P' = P + ArkScriptHash(script)·G`), appends those keys
//! to the leaf's pubkey set, and re-encodes. The result is a flat script
//! list ready for [`bitcoin::taproot::TaprootBuilder`] plus a map of which
//! leaf carries which arkade script (needed later for PSBT signing).

use crate::tapscript::ArkadeTapscript;
use crate::tapscript::TapscriptError;
use crate::tweak::compute_arkade_script_public_key;
use crate::tweak::TweakError;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::collections::BTreeMap;

/// Errors produced while processing arkade VTXO inputs.
#[derive(Debug, thiserror::Error)]
pub enum VtxoScriptError {
    #[error(transparent)]
    Tapscript(#[from] TapscriptError),
    #[error(transparent)]
    Tweak(#[from] TweakError),
}

/// An arkade-enhanced taproot leaf: a base [`ArkadeTapscript`] whose
/// pubkey set will be extended with one tweaked key per introspector,
/// each derived from `arkade_script`.
#[derive(Debug, Clone)]
pub struct ArkadeLeaf {
    pub arkade_script: ScriptBuf,
    pub tapscript: ArkadeTapscript,
    pub introspectors: Vec<XOnlyPublicKey>,
}

/// Input to [`ArkadeVtxoScript::new`]: either a plain pre-built leaf
/// script (passed through unchanged) or an [`ArkadeLeaf`].
#[derive(Debug, Clone)]
pub enum ArkadeVtxoInput {
    Plain(ScriptBuf),
    Arkade(ArkadeLeaf),
}

/// Output of arkade leaf processing.
///
/// - `scripts` — the final taproot leaf scripts, in input order.
/// - `arkade_scripts` — for each leaf that came from an [`ArkadeLeaf`], the arkade script bytes
///   keyed by leaf index. Plain leaves are absent from the map.
#[derive(Debug, Clone)]
pub struct ArkadeVtxoScript {
    pub scripts: Vec<ScriptBuf>,
    pub arkade_scripts: BTreeMap<usize, ScriptBuf>,
}

impl ArkadeVtxoScript {
    pub fn new(inputs: Vec<ArkadeVtxoInput>) -> Result<Self, VtxoScriptError> {
        let mut scripts = Vec::with_capacity(inputs.len());
        let mut arkade_scripts = BTreeMap::new();

        for input in inputs {
            let leaf_index = scripts.len();
            match input {
                ArkadeVtxoInput::Plain(script) => scripts.push(script),
                ArkadeVtxoInput::Arkade(leaf) => {
                    let tweaked_keys = leaf
                        .introspectors
                        .iter()
                        .map(|pk| compute_arkade_script_public_key(pk, &leaf.arkade_script))
                        .collect::<Result<Vec<_>, _>>()?;

                    let augmented = leaf.tapscript.with_additional_pubkeys(tweaked_keys);
                    scripts.push(augmented.encode()?);
                    arkade_scripts.insert(leaf_index, leaf.arkade_script);
                }
            }
        }

        Ok(Self {
            scripts,
            arkade_scripts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Sequence;

    // TS-derived vectors. The introspector is the secp256k1 generator G as
    // x-only. The arkade script is `OP_0 OP_INSPECTOUTPUTSCRIPTPUBKEY OP_1
    // OP_EQUALVERIFY <32 bytes 0xaa> OP_EQUAL`.
    const ARKADE_SCRIPT_HEX: &str =
        "00d1518820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa87";
    const INTRO_X_ONLY: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const EXPECTED_TWEAKED: &str =
        "a463e01f08ea4cb75a1b0756c3f06004985195db2c429bfd05e08b73be2aad85";

    const EXPECTED_LEAF_0: &str = concat!(
        "20",
        "0101010101010101010101010101010101010101010101010101010101010101",
        "ad",
        "20",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "ad",
        "20",
        "a463e01f08ea4cb75a1b0756c3f06004985195db2c429bfd05e08b73be2aad85",
        "ac",
    );
    const EXPECTED_LEAF_1: &str = concat!(
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

    fn pk(byte: u8) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&[byte; 32]).unwrap()
    }

    fn xonly(hex_str: &str) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&hex::decode(hex_str).unwrap()).unwrap()
    }

    #[test]
    fn mixed_arkade_and_plain_matches_ts() {
        let arkade_script = ScriptBuf::from_bytes(hex::decode(ARKADE_SCRIPT_HEX).unwrap());
        let plain_csv = ScriptBuf::from_bytes(hex::decode(EXPECTED_LEAF_1).unwrap());

        let result = ArkadeVtxoScript::new(vec![
            ArkadeVtxoInput::Arkade(ArkadeLeaf {
                arkade_script: arkade_script.clone(),
                tapscript: ArkadeTapscript::Multisig {
                    pubkeys: vec![pk(0x01), pk(0x02)],
                },
                introspectors: vec![xonly(INTRO_X_ONLY)],
            }),
            ArkadeVtxoInput::Plain(plain_csv),
        ])
        .unwrap();

        assert_eq!(result.scripts.len(), 2);
        assert_eq!(hex::encode(result.scripts[0].as_bytes()), EXPECTED_LEAF_0);
        assert_eq!(hex::encode(result.scripts[1].as_bytes()), EXPECTED_LEAF_1);

        // Arkade script is recorded for leaf 0 only.
        assert_eq!(result.arkade_scripts.len(), 1);
        assert_eq!(result.arkade_scripts.get(&0), Some(&arkade_script));
        assert!(!result.arkade_scripts.contains_key(&1));
    }

    #[test]
    fn tweak_appears_in_encoded_leaf() {
        // Sanity-check that the tweak we compute matches the TS vector, since
        // the leaf-0 assertion depends on it.
        let arkade_script = ScriptBuf::from_bytes(hex::decode(ARKADE_SCRIPT_HEX).unwrap());
        let tweaked =
            compute_arkade_script_public_key(&xonly(INTRO_X_ONLY), &arkade_script).unwrap();
        assert_eq!(hex::encode(tweaked.serialize()), EXPECTED_TWEAKED);
    }

    #[test]
    fn plain_only_input_passes_through() {
        let plain = ScriptBuf::from_bytes(hex::decode(EXPECTED_LEAF_1).unwrap());
        let result = ArkadeVtxoScript::new(vec![ArkadeVtxoInput::Plain(plain.clone())]).unwrap();
        assert_eq!(result.scripts, vec![plain]);
        assert!(result.arkade_scripts.is_empty());
    }

    #[test]
    fn arkade_csv_leaf_appends_tweaked_key() {
        // ArkadeLeaf whose base tapscript is a CSV multisig — the tweaked
        // introspector key is appended alongside the existing signers.
        let arkade_script = ScriptBuf::from_bytes(hex::decode(ARKADE_SCRIPT_HEX).unwrap());

        let result = ArkadeVtxoScript::new(vec![ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: arkade_script.clone(),
            tapscript: ArkadeTapscript::CsvMultisig {
                timelock: Sequence::from_height(100),
                pubkeys: vec![pk(0x01)],
            },
            introspectors: vec![xonly(INTRO_X_ONLY)],
        })])
        .unwrap();

        // Expect: <seq=100> CSV DROP <pk(0x01)> CHECKSIGVERIFY <tweaked> CHECKSIG.
        // push_int(100) in bitcoin emits a 1-byte ScriptNum push (0x01 0x64).
        let expected = concat!(
            "01",
            "64", // push 1 byte: 0x64 = 100
            "b2",
            "75", // CSV DROP
            "20",
            "0101010101010101010101010101010101010101010101010101010101010101",
            "ad",
            "20",
            "a463e01f08ea4cb75a1b0756c3f06004985195db2c429bfd05e08b73be2aad85",
            "ac",
        );
        assert_eq!(hex::encode(result.scripts[0].as_bytes()), expected);
        assert_eq!(result.arkade_scripts.get(&0), Some(&arkade_script));
    }

    #[test]
    fn multiple_arkade_leaves_all_recorded() {
        let ark1 = ScriptBuf::from_bytes(hex::decode(ARKADE_SCRIPT_HEX).unwrap());
        let ark2 = ScriptBuf::from_bytes(hex::decode("51").unwrap()); // trivial OP_1

        let result = ArkadeVtxoScript::new(vec![
            ArkadeVtxoInput::Arkade(ArkadeLeaf {
                arkade_script: ark1.clone(),
                tapscript: ArkadeTapscript::Multisig {
                    pubkeys: vec![pk(0x01)],
                },
                introspectors: vec![xonly(INTRO_X_ONLY)],
            }),
            ArkadeVtxoInput::Arkade(ArkadeLeaf {
                arkade_script: ark2.clone(),
                tapscript: ArkadeTapscript::Multisig {
                    pubkeys: vec![pk(0x02)],
                },
                introspectors: vec![xonly(INTRO_X_ONLY)],
            }),
        ])
        .unwrap();

        assert_eq!(result.arkade_scripts.len(), 2);
        assert_eq!(result.arkade_scripts.get(&0), Some(&ark1));
        assert_eq!(result.arkade_scripts.get(&1), Some(&ark2));
    }
}
