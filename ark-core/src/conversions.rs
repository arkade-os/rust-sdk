/// Utility functions for converting between Bitcoin and ZKP cryptographic types, 
/// which is necessary when working with different parts of the ARK protocol.

/// Converts a Bitcoin public key to ZKP format for use in ARK's zero-knowledge operations
pub fn to_zkp_pk(pk: PublicKey) -> zkp::PublicKey {
    zkp::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

/// Converts a ZKP x-only public key to Bitcoin format for transaction validation
pub fn from_zkp_xonly(pk: zkp::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}