//! Utilities for converting between different cryptographic types and formats.
//!
//! This module provides conversion functions between Bitcoin and ZKP (Zero-Knowledge Proof)
//! cryptographic types used throughout the ARK system. These conversions are necessary
//! when interacting with different cryptographic libraries or when moving between
//! the Bitcoin transaction layer and ARK's privacy features.

use bitcoin::secp256k1::PublicKey;
use bitcoin::XOnlyPublicKey;

/// Converts a Bitcoin public key to a ZKP public key format.
///
/// This function takes a standard Bitcoin public key and converts it to the 
/// corresponding ZKP (Zero-Knowledge Proof) public key format used in the ARK system.
///
/// # Arguments
///
/// * `pk` - A Bitcoin public key to convert
///
/// # Returns
///
/// A ZKP public key
///
/// # Panics
///
/// Panics if the conversion is invalid, which should not happen with a valid Bitcoin public key
///
/// # Examples
///
/// ```
/// use bitcoin::secp256k1::{PublicKey, Secp256k1};
/// use bitcoin::secp256k1::rand::rngs::OsRng;
/// use ark_core::conversions::to_zkp_pk;
///
/// let secp = Secp256k1::new();
/// let (_, public_key) = secp.generate_keypair(&mut OsRng);
/// let zkp_key = to_zkp_pk(public_key);
/// ```
pub fn to_zkp_pk(pk: PublicKey) -> zkp::PublicKey {
    zkp::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

/// Converts a ZKP x-only public key to a Bitcoin x-only public key.
///
/// This function takes a ZKP x-only public key and converts it to the 
/// standard Bitcoin x-only public key format. X-only public keys are 
/// commonly used in Taproot outputs and Schnorr signatures.
///
/// # Arguments
///
/// * `pk` - A ZKP x-only public key to convert
///
/// # Returns
///
/// A Bitcoin x-only public key
///
/// # Panics
///
/// Panics if the conversion is invalid, which should not happen with a valid ZKP x-only public key
///
/// # Examples
///
/// ```
/// use bitcoin::XOnlyPublicKey;
/// use zkp::{self, Secp256k1};
/// use ark_core::conversions::from_zkp_xonly;
///
/// // Create a ZKP x-only public key
/// let secp = Secp256k1::new();
/// let (sk, _) = secp.generate_keypair(&mut rand::thread_rng());
/// let zkp_xonly = zkp::XOnlyPublicKey::from_keypair(&secp, &sk).0;
///
/// // Convert to Bitcoin x-only public key
/// let btc_xonly = from_zkp_xonly(zkp_xonly);
/// ```
pub fn from_zkp_xonly(pk: zkp::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}