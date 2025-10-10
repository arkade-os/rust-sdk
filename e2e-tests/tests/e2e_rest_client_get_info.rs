// use ark_rest::Client;
// use std::time::Duration;

// #[tokio::test]
// #[ignore]
// async fn can_get_info_from_ark_server() {
//     let client = Client::new("http://localhost:7070".to_string());

//     let mut n_retries = 0;
//     while n_retries < 5 {
//         let res = client.get_info().await;

//         match res {
//             Ok(_) => {
//                 return;
//             }
//             Err(error) => {
//                 tracing::warn!(?error, "Failed to get info, retrying");

//                 tokio::time::sleep(Duration::from_secs(2)).await;

//                 n_retries += 1;

//                 continue;
//             }
//         };
//     }

//     panic!("Failed to get info after several retries");
// }
