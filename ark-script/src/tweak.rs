//! Arkade script key tweaking.
//!
//! The introspector co-signing service binds a base public key `P` to an
//! arkade script by deriving
//!
//! ```text
//! P' = P + taggedHash("ArkScriptHash", script) * G
//! ```
//!
//! This is **not** BIP-341 taproot tweaking — it is a plain elliptic-curve
//! point addition. The input `P` is an x-only public key (even-Y is
//! enforced, matching the introspector's Go implementation which
//! round-trips through `schnorr.SerializePubKey`).

use bitcoin::hashes::sha256t_hash_newtype;
use bitcoin::hashes::Hash;
use bitcoin::key::Parity;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::Scalar;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::Script;
use bitcoin::XOnlyPublicKey;

sha256t_hash_newtype! {
    pub struct ArkScriptHashTag = hash_str("ArkScriptHash");

    /// BIP-340 tagged hash of an arkade script, used to tweak introspector
    /// public keys.
    #[hash_newtype(forward)]
    pub struct ArkadeScriptHash(_);

    pub struct ArkWitnessHashTag = hash_str("ArkWitnessHash");

    /// BIP-340 tagged hash of an arkade witness.
    #[hash_newtype(forward)]
    pub struct ArkadeWitnessHash(_);
}

/// Errors returned by [`compute_arkade_script_public_key`].
#[derive(Debug, thiserror::Error)]
pub enum TweakError {
    #[error("arkade script hash is zero")]
    ZeroHash,
    #[error("arkade script hash is greater than or equal to the curve order")]
    HashOutOfRange,
    #[error("tweak resulted in the point at infinity")]
    Identity,
}

/// BIP-340 tagged hash of an arkade script with tag `"ArkScriptHash"`.
pub fn arkade_script_hash(script: &Script) -> [u8; 32] {
    ArkadeScriptHash::hash(script.as_bytes()).to_byte_array()
}

/// BIP-340 tagged hash of an arkade witness with tag `"ArkWitnessHash"`.
///
/// Returns all zeros for an empty witness, matching the Go introspector's
/// sentinel for "no witness".
pub fn arkade_witness_hash(witness: &[u8]) -> [u8; 32] {
    if witness.is_empty() {
        return [0u8; 32];
    }
    ArkadeWitnessHash::hash(witness).to_byte_array()
}

/// Compute the introspector-bound public key for an arkade script:
///
/// ```text
/// P' = P + ArkScriptHash(script) * G
/// ```
///
/// `P` is x-only so even-Y is enforced on both inputs and output.
///
/// The hash is interpreted as a big-endian scalar. The astronomically
/// unlikely pathological cases (`hash == 0`, `hash >= n`, or `P' == ∞`) all
/// surface as errors rather than silently substituting another value, so
/// the result either matches the TS implementation exactly or fails
/// visibly.
pub fn compute_arkade_script_public_key(
    pubkey: &XOnlyPublicKey,
    script: &Script,
) -> Result<XOnlyPublicKey, TweakError> {
    let secp = Secp256k1::verification_only();

    let hash = arkade_script_hash(script);
    let scalar = Scalar::from_be_bytes(hash).map_err(|_| TweakError::HashOutOfRange)?;
    if scalar == Scalar::ZERO {
        return Err(TweakError::ZeroHash);
    }

    let base = PublicKey::from_x_only_public_key(*pubkey, Parity::Even);
    let tweaked = base
        .add_exp_tweak(&secp, &scalar)
        .map_err(|_| TweakError::Identity)?;

    Ok(tweaked.x_only_public_key().0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::ScriptBuf;

    // Vectors generated with @noble/curves/secp256k1 matching the TS
    // implementation at ts-sdk/src/arkade/tweak.ts on branch
    // arkade-script-final.
    const SCRIPT_00AC: &str = "00ac";
    const SCRIPT_P2PKH: &str = "76a9141234567890abcdef1234567890abcdef1234567888ac";

    const EXPECTED_SCRIPT_HASH_00AC: &str =
        "daaf1b35c7f10014d37ef3c977a713de7ccb70a79df1e5fd875f453a5fe5ab40";
    const EXPECTED_SCRIPT_HASH_P2PKH: &str =
        "98f61ad1b41b1774de27ae8729815fb924094c697493d5a7ac188b47ab6c42ae";
    const EXPECTED_WITNESS_HASH_010203: &str =
        "de2847a382fb70acc89784a30206854f2d698bfaab5ba18d8b1af907d3bd6ea7";

    // x-coordinate of secp256k1 generator point G.
    const GEN_X: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const ONES_32: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    const EXPECTED_TWEAK_G_00AC: &str =
        "fda5affba44d470c8f0975c685cb27e5d3aabb41fbeace50dfac741d5fdc5fd8";
    const EXPECTED_TWEAK_G_P2PKH: &str =
        "7663e8781d6e04bad4d277234e2223d3702e0a78dcd1ec24d0e06302aec1d0d5";
    const EXPECTED_TWEAK_ONES_00AC: &str =
        "d088cce863079f12d66b64b265dc36bfb6429ecf0962877d94a4df85061e2bf6";

    fn decode_hex(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    fn script(hex_bytes: &str) -> ScriptBuf {
        ScriptBuf::from_bytes(decode_hex(hex_bytes))
    }

    fn xonly(hex_bytes: &str) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&decode_hex(hex_bytes)).unwrap()
    }

    #[test]
    fn script_hash_matches_ts_vector() {
        assert_eq!(
            hex::encode(arkade_script_hash(&script(SCRIPT_00AC))),
            EXPECTED_SCRIPT_HASH_00AC
        );
        assert_eq!(
            hex::encode(arkade_script_hash(&script(SCRIPT_P2PKH))),
            EXPECTED_SCRIPT_HASH_P2PKH
        );
    }

    #[test]
    fn witness_hash_empty_is_zero() {
        assert_eq!(arkade_witness_hash(&[]), [0u8; 32]);
    }

    #[test]
    fn witness_hash_matches_ts_vector() {
        assert_eq!(
            hex::encode(arkade_witness_hash(&[0x01, 0x02, 0x03])),
            EXPECTED_WITNESS_HASH_010203
        );
    }

    #[test]
    fn witness_hash_differs_between_inputs() {
        let a = arkade_witness_hash(&[0x01]);
        let b = arkade_witness_hash(&[0x02]);
        assert_ne!(a, b);
    }

    #[test]
    fn witness_hash_differs_from_script_hash_for_same_bytes() {
        let data = [0x01u8, 0x02, 0x03];
        let sh = arkade_script_hash(Script::from_bytes(&data));
        let wh = arkade_witness_hash(&data);
        assert_ne!(sh, wh);
    }

    #[test]
    fn tweak_matches_ts_vector_generator_short_script() {
        let tweaked =
            compute_arkade_script_public_key(&xonly(GEN_X), &script(SCRIPT_00AC)).unwrap();
        assert_eq!(hex::encode(tweaked.serialize()), EXPECTED_TWEAK_G_00AC);
    }

    #[test]
    fn tweak_matches_ts_vector_generator_p2pkh() {
        let tweaked =
            compute_arkade_script_public_key(&xonly(GEN_X), &script(SCRIPT_P2PKH)).unwrap();
        assert_eq!(hex::encode(tweaked.serialize()), EXPECTED_TWEAK_G_P2PKH);
    }

    #[test]
    fn tweak_matches_ts_vector_non_generator() {
        let tweaked =
            compute_arkade_script_public_key(&xonly(ONES_32), &script(SCRIPT_00AC)).unwrap();
        assert_eq!(hex::encode(tweaked.serialize()), EXPECTED_TWEAK_ONES_00AC);
    }

    #[test]
    fn tweak_is_deterministic() {
        let a = compute_arkade_script_public_key(&xonly(GEN_X), &script(SCRIPT_00AC)).unwrap();
        let b = compute_arkade_script_public_key(&xonly(GEN_X), &script(SCRIPT_00AC)).unwrap();
        assert_eq!(a, b);
    }
}
