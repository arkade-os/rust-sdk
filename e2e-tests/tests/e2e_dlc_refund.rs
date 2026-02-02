#![allow(clippy::unwrap_used)]

use common::Nigiri;
use std::sync::Arc;

mod common;
mod dlc_common;

#[tokio::test]
#[ignore]
pub async fn e2e_dlc_refund() {
    common::init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    dlc_common::run_dlc_scenario(&nigiri, true).await.unwrap();
}
