#[ignore] // we want to run this test only manually as it needs a full ark server
#[wasm_bindgen_test::wasm_bindgen_test]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
async fn test_get_info() {
    use ark_rest::Client;

    let server_url = "http://localhost:7070".to_string();

    let client = Client::new(server_url);

    match client.get_info().await {
        Ok(info) => {
            assert!(info.round_interval > 0, "Round interval should be positive");

            web_sys::console::log_1(&format!("Got info: {info:?}").into());
        }
        Err(err) => {
            web_sys::console::error_1(&format!("Error getting info: {err:?}").into());
            panic!("get_info failed with error: {err:?}");
        }
    }
}
