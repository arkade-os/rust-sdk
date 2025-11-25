use bitcoin::XOnlyPublicKey;
use bitcoin::secp256k1::PublicKey;

pub fn to_musig_pk(pk: PublicKey) -> musig::PublicKey {
    musig::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

pub fn from_musig_xonly(pk: musig::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}
