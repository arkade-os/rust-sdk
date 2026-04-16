#![allow(clippy::unwrap_used)]

use ark_delegator::DelegatorClient;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client_with_delegator;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
async fn e2e_fulmine_delegator_smoke() {
    init_tracing();

    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();

    // Fulmine delegator API (local regtest stack).
    let delegator = Arc::new(DelegatorClient::new("http://localhost:7004".to_string()));
    let info = delegator.info().await.unwrap();

    let delegator_pk: bitcoin::PublicKey = info.pubkey.parse().unwrap();
    let delegator_pk: bitcoin::XOnlyPublicKey = delegator_pk.into();

    let (client, _) = set_up_client_with_delegator(
        "alice-delegator-smoke".to_string(),
        nigiri.clone(),
        secp,
        delegator_pk,
    )
    .await;

    let client = Arc::new(client);

    // Sanity: configured client returns delegated (3-leaf) addresses.
    let (_, next_vtxo) = client.get_offchain_address().unwrap();
    assert_eq!(next_vtxo.delegator_pk(), Some(delegator_pk));

    // Start watcher and keep handle alive for the test duration.
    let _watcher = client.start_vtxo_watcher(delegator);

    let boarding_address = client.get_boarding_address().unwrap();
    let fund_amount = Amount::from_sat(100_000);
    let _outpoint = nigiri.faucet_fund(&boarding_address, fund_amount).await;

    let mut rng = thread_rng();
    client.settle(&mut rng).await.unwrap();

    tokio::time::sleep(Duration::from_secs(5)).await;

    let (vtxo_list, script_map) = client.list_vtxos().await.unwrap();

    tracing::info!(?vtxo_list, "VTXOs after settlement");

    let has_unspent_delegated_vtxo = vtxo_list.all_unspent().any(|v| {
        script_map
            .get(&v.script)
            .is_some_and(|full_vtxo| full_vtxo.delegator_pk() == Some(delegator_pk))
    });

    assert!(
        has_unspent_delegated_vtxo,
        "expected at least one unspent delegated VTXO after settlement"
    );
}
