#![allow(clippy::unwrap_used)]

use crate::common::InMemoryDb;
use crate::common::Nigiri;
use ark_bdk_wallet::Wallet;
use ark_client::Blockchain;
use ark_client::OfflineClient;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Amount;
use bitcoin::Network;
use common::init_tracing;
use rand::thread_rng;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
pub async fn boltz() {
    init_tracing();

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let sk = SecretKey::new(&mut rng);
    let kp = Keypair::from_secret_key(&secp, &sk);

    let db = InMemoryDb::default();
    let wallet = Wallet::new(
        kp,
        secp,
        Network::Regtest,
        "https://mutinynet.arkade.sh/v1",
        db,
    )
    .unwrap();
    let wallet = Arc::new(wallet);

    let client = OfflineClient::new(
        "boltz".to_string(),
        kp,
        Arc::new(Nigiri::new()),
        wallet.clone(),
        "http://localhost:7070".to_string(),
        Duration::from_secs(30),
    )
    .connect()
    .await
    .unwrap();

    client
        .create_ln_invoice(Amount::from_sat(10_000), "hello".to_string())
        .await
        .unwrap();
}

struct Foo {}
