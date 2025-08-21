use ark_core::Vtxo;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::SecretKey;

// Configure WASM tests to run in browser environment
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test::wasm_bindgen_test]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
async fn test_get_info() {
    use ark_rest::Client;

    let server_url = "http://localhost:7070".to_string();

    let client = Client::new(server_url);

    match client.get_info().await {
        Ok(info) => {
            assert!(info.round_interval > 0, "Round interval should be positive");
        }
        Err(err) => {
            web_sys::console::error_1(&format!("Error getting info: {err:?}").into());
        }
    }
}

#[wasm_bindgen_test::wasm_bindgen_test]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
async fn test_get_offchain_address() {
    use ark_rest::Client;

    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
    let pk = PublicKey::from_secret_key(&secp, &sk);

    let server_url = "http://localhost:7070".to_string();

    let client = Client::new(server_url);

    let server_info = client
        .get_info()
        .await
        .expect("to be able to retrieve server info");

    let vtxo = Vtxo::new(
        &secp,
        server_info.pk.x_only_public_key().0,
        pk.x_only_public_key().0,
        vec![],
        server_info.unilateral_exit_delay,
        server_info.network,
    )
    .expect("to be able to create a vtxo");

    let ark_address = vtxo.to_ark_address();
    let address_string = ark_address.encode();
    assert_eq!(address_string, "tark1qqv6a8wylu3mdllwr20hwvcx89aslhctyp6eranf8pmqau97rtze0qdgw2t9vuuqwhzsm4qfvjyatltmefu8q0gh02m535j9w85dhanp087p46");
}
