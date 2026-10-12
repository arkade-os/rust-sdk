#![allow(clippy::unwrap_used)]

use ark_bdk_wallet::Wallet;
use ark_client::Client;
use ark_client::InMemorySwapStorage;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use bitcoin::OutPoint;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::sync::Arc;
use std::time::Duration;

mod common;

type TestClient = Client<Regtest, Wallet, InMemorySwapStorage>;

/// Long enough for a settlement to register its intent, well short of the next batch.
const REGISTRATION_GRACE: Duration = Duration::from_millis(500);

/// A client holding one VTXO: its boarding output settled into a batch.
async fn client_with_vtxo(name: &str) -> (Arc<TestClient>, OutPoint) {
    let regtest = Arc::new(Regtest::new());
    let (client, _) = set_up_client(name.to_string(), regtest.clone(), Secp256k1::new()).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest
        .faucet_fund(&boarding_address, Amount::ONE_BTC)
        .await;
    let mut rng = StdRng::from_entropy();
    client.settle(&mut rng).await.unwrap().unwrap();

    let vtxo = client
        .list_vtxos()
        .await
        .unwrap()
        .spendable_offchain()
        .next()
        .unwrap()
        .vtxo()
        .outpoint;

    (Arc::new(client), vtxo)
}

/// Start settling `vtxo` and abandon the settlement once its intent is registered, as a client
/// that crashes or stops waiting does: the intent stays queued at the server, holding `vtxo`.
async fn abandon_settlement(client: Arc<TestClient>, vtxo: OutPoint) {
    let handle = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        let _ = client.settle_vtxos(&mut rng, &[vtxo], &[]).await;
    });
    tokio::time::sleep(REGISTRATION_GRACE).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
#[ignore]
pub async fn delete_intents_releases_a_vtxo_held_by_an_abandoned_intent() {
    init_tracing();
    let (alice, vtxo) = client_with_vtxo("alice").await;

    abandon_settlement(alice.clone(), vtxo).await;

    alice.delete_intents(&[vtxo]).await.unwrap();

    let mut rng = StdRng::from_entropy();
    let txid = alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();
    assert!(txid.is_some(), "the VTXO should settle");
}

#[tokio::test]
#[ignore]
pub async fn settle_replaces_its_own_abandoned_intent() {
    init_tracing();
    let (alice, vtxo) = client_with_vtxo("alice").await;

    abandon_settlement(alice.clone(), vtxo).await;

    // The abandoned intent still holds the VTXO; settling deletes it and retries.
    let mut rng = StdRng::from_entropy();
    let txid = alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();
    assert!(txid.is_some(), "the VTXO should settle");
}

#[tokio::test]
#[ignore]
pub async fn settle_replaces_an_abandoned_intent_that_batches_keep_requeuing() {
    init_tracing();
    let (alice, vtxo) = client_with_vtxo("alice").await;

    abandon_settlement(alice.clone(), vtxo).await;
    // Every batch takes the abandoned intent, gets no confirmation for it and queues it again,
    // whatever its expiry: it never clears by itself.
    tokio::time::sleep(Duration::from_secs(75)).await;

    let mut rng = StdRng::from_entropy();
    let txid = alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();
    assert!(txid.is_some(), "the VTXO should settle");
}
