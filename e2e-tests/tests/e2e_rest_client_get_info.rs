#![allow(clippy::unwrap_used)]

use ark_rest::Client;
use futures::StreamExt;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
async fn can_get_info_from_ark_server() {
    common::init_tracing();

    let client = Client::new("http://localhost:7070".to_string());

    let mut n_retries = 0;
    while n_retries < 5 {
        let res = client.get_info().await;

        match res {
            Ok(info) => {
                tracing::info!(?info, "Got info from ark server");
                return;
            }
            Err(error) => {
                tracing::warn!(?error, "Failed to get info, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                n_retries += 1;
                continue;
            }
        };
    }

    panic!("Failed to get info after several retries");
}

#[tokio::test]
#[ignore]
async fn can_receive_events_from_event_stream() {
    common::init_tracing();

    let client = Client::new("http://localhost:7070".to_string());

    // First verify the server is reachable.
    let info = client.get_info().await.unwrap();
    tracing::info!(?info, "Connected to ark server");

    // Subscribe to the event stream with no topic filters.
    let mut event_stream = client.get_event_stream(vec![]).await.unwrap();

    // Wait for the first successfully parsed event (typically a heartbeat).
    let first_event = tokio::time::timeout(Duration::from_secs(65), event_stream.next())
        .await
        .expect("timed out waiting for first event from event stream")
        .expect("event stream ended unexpectedly")
        .expect("failed to parse event from stream");

    tracing::info!(?first_event, "Received event — SSE parsing works correctly");
}
