use crate::Error;
use bech32::Bech32m;
use bech32::Hrp;
use bitcoin::key::TweakedPublicKey;
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArkAddress {
    version: u8,
    hrp: Hrp,
    server: XOnlyPublicKey,
    vtxo_tap_key: TweakedPublicKey,
}

impl ArkAddress {
    pub fn to_p2tr_script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_p2tr_tweaked(self.vtxo_tap_key)
    }

    pub fn to_sub_dust_script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_op_return(self.vtxo_tap_key.serialize())
    }
}

impl ArkAddress {
    pub fn new(network: Network, server: XOnlyPublicKey, vtxo_tap_key: TweakedPublicKey) -> Self {
        let hrp = match network {
            Network::Bitcoin => "ark",
            Network::Testnet | Network::Testnet4 | Network::Signet | Network::Regtest => "tark",
        };

        let hrp = Hrp::parse_unchecked(hrp);

        Self {
            version: 0,
            hrp,
            server,
            vtxo_tap_key,
        }
    }

    pub fn encode(&self) -> String {
        let mut bytes = [0u8; 65];

        bytes[0] = self.version;

        bytes[1..33].copy_from_slice(&self.server.serialize());
        bytes[33..].copy_from_slice(&self.vtxo_tap_key.serialize());

        bech32::encode::<Bech32m>(self.hrp, bytes.as_slice()).expect("data can be encoded")
    }

    pub fn decode(value: &str) -> Result<Self, Error> {
        let (hrp, bytes) = bech32::decode(value).map_err(Error::address_format)?;

        // ArkAddress requires exactly 65 bytes: 1 version + 32 server pubkey + 32 vtxo pubkey
        if bytes.len() != 65 {
            return Err(Error::address_format(format!(
                "invalid ark address length: expected 65 bytes, got {}",
                bytes.len()
            )));
        }

        let version = bytes[0];

        let server = XOnlyPublicKey::from_slice(&bytes[1..33]).map_err(Error::address_format)?;
        let vtxo_tap_key =
            XOnlyPublicKey::from_slice(&bytes[33..]).map_err(Error::address_format)?;

        // It is safe to call `dangerous_assume_tweaked` because we are treating the VTXO tap key as
        // finished product i.e. we are only going to use it as an address to send coins to.
        let vtxo_tap_key = TweakedPublicKey::dangerous_assume_tweaked(vtxo_tap_key);

        Ok(Self {
            version,
            hrp,
            server,
            vtxo_tap_key,
        })
    }
}

impl std::fmt::Display for ArkAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for ArkAddress {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::decode(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hex::DisplayHex;

    // Taken from https://github.com/arkade-os/arkd/blob/b2c4a6ea6ab1a5a4078c578bcfca650ed19dc4ec/common/fixtures/encoding.json.
    #[test]
    fn roundtrip() {
        let address = "tark1qqellv77udfmr20tun8dvju5vgudpf9vxe8jwhthrkn26fz96pawqfdy8nk05rsmrf8h94j26905e7n6sng8y059z8ykn2j5xcuw4xt846qj6x";

        let decoded = ArkAddress::decode(address).unwrap();

        let hrp = decoded.hrp.to_string();
        assert_eq!(hrp, "tark");

        let version = decoded.version;
        assert_eq!(version, 0);

        let server = decoded.server.serialize().as_hex().to_string();
        assert_eq!(
            server,
            "33ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0"
        );

        let vtxo_tap_key = decoded.vtxo_tap_key.serialize().as_hex().to_string();
        assert_eq!(
            vtxo_tap_key,
            "25a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967"
        );

        let encoded = decoded.encode();

        assert_eq!(encoded, address);
    }
}
