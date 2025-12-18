use bitcoin::secp256k1::PublicKey;
use bitcoin::XOnlyPublicKey;

pub fn to_musig_pk(pk: PublicKey) -> musig::PublicKey {
    musig::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

pub fn from_musig_xonly(pk: musig::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}
