#![allow(clippy::unwrap_used)]

use ark_client::error::Error;
use ark_client::wallet::Persistence;
use ark_core::BoardingOutput;
use bitcoin::secp256k1::SecretKey;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct InMemoryDb {
    boarding_outputs: RwLock<HashMap<BoardingOutput, SecretKey>>,
}

impl Persistence for InMemoryDb {
    fn save_boarding_output(
        &self,
        sk: SecretKey,
        boarding_output: BoardingOutput,
    ) -> Result<(), Error> {
        self.boarding_outputs
            .write()
            .unwrap()
            .insert(boarding_output, sk);

        Ok(())
    }

    fn load_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, Error> {
        Ok(self
            .boarding_outputs
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect())
    }

    fn sk_for_pk(&self, pk: &XOnlyPublicKey) -> Result<SecretKey, Error> {
        let maybe_sk = self
            .boarding_outputs
            .read()
            .unwrap()
            .iter()
            .find_map(|(b, sk)| if b.owner_pk() == *pk { Some(*sk) } else { None });
        let secret_key = maybe_sk.unwrap();
        Ok(secret_key)
    }
}
