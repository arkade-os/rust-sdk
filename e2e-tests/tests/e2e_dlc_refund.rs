#![allow(clippy::unwrap_used)]

use common::Regtest;
use std::sync::Arc;

mod common;
mod dlc_common;

#[tokio::test]
#[ignore]
pub async fn e2e_dlc_refund() {
    common::init_tracing();
    let regtest = Arc::new(Regtest::new());

    dlc_common::run_dlc_scenario(&regtest, true).await.unwrap();
}
