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

/// The admin API of the regtest arkd.
const ARKD_ADMIN_URL: &str = "http://localhost:7071";

/// How long to wait for an intent to show up at the server: longer than a batch, which takes an
/// intent out of the queue while it runs.
const INTENT_WAIT: Duration = Duration::from_secs(90);

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
    wait_for_queued_intent(vtxo).await;
    handle.abort();
    let _ = handle.await;
}

/// Wait until an intent queued at the server spends `vtxo`.
async fn wait_for_queued_intent(vtxo: OutPoint) {
    tokio::time::timeout(INTENT_WAIT, async {
        while !intent_is_queued(vtxo).await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no intent spending {vtxo} was queued"));
}

/// Whether an intent queued at the server spends `vtxo`, as reported by arkd's admin API.
async fn intent_is_queued(vtxo: OutPoint) -> bool {
    #[derive(serde::Deserialize)]
    struct ListIntentsResponse {
        #[serde(default)]
        intents: Vec<IntentInfo>,
    }

    #[derive(serde::Deserialize)]
    struct IntentInfo {
        #[serde(default)]
        inputs: Vec<IntentInput>,
    }

    #[derive(serde::Deserialize)]
    struct IntentInput {
        txid: String,
        #[serde(default)]
        vout: u32,
    }

    let response: ListIntentsResponse = reqwest::get(format!("{ARKD_ADMIN_URL}/v1/admin/intents"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    response.intents.iter().any(|intent| {
        intent
            .inputs
            .iter()
            .any(|input| input.txid == vtxo.txid.to_string() && input.vout == vtxo.vout)
    })
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
    wait_for_queued_intent(vtxo).await;
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
    wait_for_queued_intent(vtxo).await;

    let mut rng = StdRng::from_entropy();
    let txid = alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();
    assert!(txid.is_some(), "the VTXO should settle");
}
