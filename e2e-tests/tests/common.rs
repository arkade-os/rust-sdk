#![allow(clippy::unwrap_used)]

use ark_bdk_wallet::Wallet;
use ark_client::error::Error;
use ark_client::lightning_invoice::Bolt11Invoice;
use ark_client::Blockchain;
use ark_client::Client;
use ark_client::InMemorySwapStorage;
use ark_client::OfflineClient;
use ark_client::OfflineClientConfig;
use ark_client::SpendStatus;
use ark_client::TxStatus;
use ark_core::ExplorerUtxo;
use base64::Engine;
use bitcoin::bip32::Xpriv;
use bitcoin::hex::FromHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use rand::thread_rng;
use rand::Rng;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Arc;
use std::sync::Once;
use std::sync::RwLock;
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct BitcoinRpc {
    url: String,
    username: String,
    password: String,
    reqwest_client: reqwest::Client,
}

impl BitcoinRpc {
    pub fn new(url: String, username: String, password: String) -> Self {
        Self {
            url,
            username,
            password,
            reqwest_client: reqwest::Client::new(),
        }
    }

    pub async fn submit_package(&self, txs: Vec<String>) -> Result<(), Error> {
        let rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "submitpackage",
            "params": [txs]
        });

        let response = self
            .reqwest_client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .basic_auth(&self.username, Some(&self.password))
            .json(&rpc_request)
            .send()
            .await
            .map_err(Error::wallet)?;

        let status = response.status();
        let response_text = response.text().await.map_err(Error::wallet)?;

        if !status.is_success() {
            return Err(Error::wallet(format!(
                "Bitcoin RPC request failed with status {status}: {response_text}",
            )));
        }

        if response_text.contains("failed") {
            return Err(Error::wallet(format!(
                "Bitcoin RPC submitpackage failed: {response_text}",
            )));
        }

        // Parse JSON-RPC response to check for RPC-level errors
        let rpc_response: serde_json::Value = serde_json::from_str(&response_text)
            .map_err(|e| Error::wallet(format!("Failed to parse RPC response: {e}")))?;

        if let Some(error) = rpc_response.get("error") {
            return Err(Error::wallet(format!(
                "Bitcoin RPC submitpackage error: {error}",
            )));
        }

        tracing::debug!(
            "Successfully submitted package of {} transactions",
            txs.len()
        );
        Ok(())
    }

    /// Make a blocking JSON-RPC call and return its `result` field.
    ///
    /// Several `Blockchain` lookups go straight to Bitcoin Core instead of the
    /// esplora indexer because mempool's esplora-compatible API lags behind the
    /// chain on regtest. That lag caused two distinct e2e failures: the client
    /// re-spending an already-spent boarding output (arkd: "boarding input ...
    /// is spent"), and not finding a freshly-mined commitment TX when building
    /// unilateral exit trees. The node's own view is authoritative and lag-free.
    ///
    /// These are blocking calls (used from the synchronous `Blockchain`
    /// lookups), so they go through `minreq` rather than the async reqwest
    /// client.
    ///
    /// Returns the `result` field: `Ok(None)` for a genuine null result (e.g.
    /// `gettxout` on an unknown/spent output, or an unknown tx), and `Err` for a
    /// transport/parse failure — kept distinct so callers don't mistake an RPC
    /// hiccup for an authoritative "not found".
    fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Option<serde_json::Value>, Error> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();

        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.username, self.password));

        let value = minreq::post(&self.url)
            .with_header("Content-Type", "application/json")
            .with_header("Authorization", format!("Basic {auth}"))
            .with_body(body)
            .send()
            .map_err(|e| Error::wallet(format!("Bitcoin RPC transport error: {e}")))?
            .json::<serde_json::Value>()
            .map_err(|e| Error::wallet(format!("Bitcoin RPC parse error: {e}")))?;

        Ok(value
            .get("result")
            .filter(|result| !result.is_null())
            .cloned())
    }

    /// Whether `txid:vout` is still in the node's UTXO set (unspent), accounting
    /// for the mempool. `gettxout` returns a null result for an unknown or
    /// already-spent output.
    fn is_output_unspent(&self, txid: &Txid, vout: u32) -> bool {
        // include_mempool = true: treat outputs spent by an unconfirmed tx as
        // spent too.
        match self.rpc_call(
            "gettxout",
            serde_json::json!([txid.to_string(), vout, true]),
        ) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            // On an RPC hiccup, fall back to "unspent" so we don't silently drop
            // a genuinely spendable output.
            Err(e) => {
                tracing::warn!(%txid, vout, error = %e, "gettxout failed; treating as unspent");
                true
            }
        }
    }

    /// Fetch a transaction by id from the node (mempool or, via `txindex`, a
    /// confirmed block). `None` if the node has never seen it (or on RPC error).
    fn get_raw_transaction(&self, txid: &Txid) -> Option<Transaction> {
        let hex = self
            .rpc_call("getrawtransaction", serde_json::json!([txid.to_string()]))
            .ok()
            .flatten()?;
        let bytes = Vec::from_hex(hex.as_str()?).ok()?;
        bitcoin::consensus::deserialize(&bytes).ok()
    }

    /// The block time a transaction was confirmed at, or `None` if it is
    /// unconfirmed or unknown (or on RPC error).
    fn get_tx_blocktime(&self, txid: &Txid) -> Option<i64> {
        // verbose = true returns a JSON object that includes `blocktime` once
        // the transaction is mined.
        self.rpc_call(
            "getrawtransaction",
            serde_json::json!([txid.to_string(), true]),
        )
        .ok()
        .flatten()?
        .get("blocktime")?
        .as_i64()
    }
}

/// Resolve the path to the arkade-regtest `regtest.mjs` orchestrator CLI.
///
/// Defaults to `<workspace root>/regtest/regtest.mjs` (the `regtest` submodule),
/// computed from this crate's manifest dir so it works regardless of the test's
/// working directory. Override with `REGTEST_DIR` if the submodule lives
/// elsewhere.
#[allow(unused)]
fn regtest_mjs_path() -> PathBuf {
    if let Ok(dir) = std::env::var("REGTEST_DIR") {
        return PathBuf::from(dir).join("regtest.mjs");
    }

    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("e2e-tests crate has a parent (workspace root)")
        .join("regtest")
        .join("regtest.mjs")
}

/// Drives the arkade-regtest Docker Compose stack (Bitcoin Core + Fulcrum +
/// mempool/esplora + arkd + …) via its zero-dependency Node CLI (`regtest.mjs`).
///
/// Replaces the old `nigiri` binary: faucet/mine shell out to `regtest.mjs`,
/// chain queries go through mempool's Esplora-compatible REST API, and package
/// submission hits Bitcoin Core RPC directly.
pub struct Regtest {
    esplora_client: esplora_client::BlockingClient,
    /// By how much we _reduce_ the block time of outpoints.
    ///
    /// This can be used to ensure that certain outpoints are considered spendable, which is useful
    /// for testing scripts with opcodes such as `OP_CSV`.
    outpoint_blocktime_offset: RwLock<u64>,
    /// By how much we _reduce_ the block height of outpoints.
    ///
    /// This can be used to ensure that certain outpoints are considered spendable, which is useful
    /// for testing scripts with opcodes such as `OP_CSV`.
    outpoint_block_height_offset: RwLock<u64>,
    /// Bitcoin RPC client for package submission
    bitcoin_rpc: BitcoinRpc,
}

impl Regtest {
    pub fn new() -> Self {
        // mempool serves the Esplora-compatible REST API under `/api`.
        let esplora_url = "http://localhost:3000/api";
        let bitcoin_rpc = BitcoinRpc::new(
            "http://localhost:18443".to_string(),
            "admin1".to_string(),
            "123".to_string(),
        );

        let builder = esplora_client::Builder::new(esplora_url);
        let esplora_client = builder.build_blocking();

        Self {
            esplora_client,
            outpoint_blocktime_offset: RwLock::new(0),
            outpoint_block_height_offset: RwLock::new(0),
            bitcoin_rpc,
        }
    }

    /// Run a `regtest.mjs` subcommand, asserting it succeeds.
    #[allow(unused)]
    fn run_regtest(&self, args: &[&str]) -> Output {
        let script = regtest_mjs_path();
        let output = Command::new("node")
            .arg(&script)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `node {} {args:?}`: {e}", script.display()));

        assert!(
            output.status.success(),
            "`regtest.mjs {args:?}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        output
    }

    #[allow(unused)]
    pub async fn faucet_fund(&self, address: &Address, amount: Amount) -> OutPoint {
        // Snapshot the address's existing outpoints so we can tell the new
        // faucet output apart from any pre-existing one of the same amount (the
        // faucet doesn't print the funding txid for us to match on directly).
        let existing: std::collections::HashSet<OutPoint> = self
            .find_outpoints_blocking(address)
            .into_iter()
            .map(|utxo| utxo.outpoint)
            .collect();

        // `--confirm` mines one block so the funds confirm immediately; unlike
        // nigiri, the regtest CLI does not auto-mine on faucet.
        self.run_regtest(&[
            "faucet",
            &address.to_string(),
            &amount.to_btc().to_string(),
            "--confirm",
        ]);

        // Locate the newly created output via esplora. Poll until the indexer
        // catches up with the freshly mined block.
        for _ in 0..30 {
            if let Some(utxo) = self
                .find_outpoints_blocking(address)
                .into_iter()
                .find(|u| u.amount == amount && !u.is_spent && !existing.contains(&u.outpoint))
            {
                return utxo.outpoint;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        panic!("funding output for {address} ({amount}) not found via esplora");
    }

    #[allow(unused)]
    pub fn set_outpoint_blocktime_offset(&self, outpoint_blocktime_offset: u64) {
        let mut guard = self.outpoint_blocktime_offset.write().unwrap();
        *guard = outpoint_blocktime_offset;
    }

    #[allow(unused)]
    pub fn set_outpoint_block_height_offset(&self, outpoint_block_height_offset: u64) {
        let mut guard = self.outpoint_block_height_offset.write().unwrap();
        *guard = outpoint_block_height_offset;
    }

    /// Rotate the arkd signing key. `cutoff` is passed verbatim to `--cutoff`:
    /// use `"+86400"` for a future cutoff (regime 1, still cooperative) or
    /// `"-60"` for a past cutoff (regime 2, operator won't co-sign).
    #[allow(unused)]
    pub fn rotate_signer(&self, cutoff: &str) {
        self.run_regtest(&["rotate-signer", "--cutoff", cutoff]);
    }

    // `mine` stays async (callers `.await` it) even though shelling out to the
    // regtest CLI is synchronous.
    #[allow(unused, clippy::unused_async)]
    pub async fn mine(&self, n: u32) {
        self.run_regtest(&["mine", &n.to_string()]);

        tracing::debug!(n, "Mined blocks");
    }
}

impl Default for Regtest {
    fn default() -> Self {
        Self::new()
    }
}

impl Regtest {
    /// Get the current block height from the esplora client.
    #[allow(unused)]
    pub fn get_height(&self) -> u32 {
        self.esplora_client.get_height().unwrap()
    }

    /// Synchronous version of outpoint lookup. The underlying esplora client is blocking, so this
    /// can be called from non-async contexts (e.g. closures passed to `list_boarding_outpoints`).
    pub fn find_outpoints_blocking(&self, address: &Address) -> Vec<ExplorerUtxo> {
        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .unwrap();

        let current_block_height = self.get_height();

        let outpoint_blocktime_offset = {
            let guard = self.outpoint_blocktime_offset.read();
            *guard.unwrap()
        };

        let outpoint_block_height_offset = {
            let guard = self.outpoint_block_height_offset.read();
            *guard.unwrap()
        };

        let outputs = txs
            .into_iter()
            .flat_map(|tx| {
                let txid = tx.txid;

                let confirmation_blocktime =
                    tx.status.block_time.map(|t| t - outpoint_blocktime_offset);

                let confirmations = match tx.status.block_height {
                    Some(confirmation_block_height) => {
                        match current_block_height.checked_sub(
                            confirmation_block_height
                                .saturating_sub(outpoint_block_height_offset as u32),
                        ) {
                            Some(x) => x + 1,
                            None => 0,
                        }
                    }
                    None => 0,
                };

                tx.vout
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.scriptpubkey == script_pubkey)
                    .map(|(i, v)| ExplorerUtxo {
                        outpoint: OutPoint {
                            txid,
                            vout: i as u32,
                        },
                        amount: Amount::from_sat(v.value),
                        confirmation_blocktime,
                        confirmations: confirmations as u64,
                        // Assume the output is unspent until we dig deeper, further down.
                        is_spent: false,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut utxos = Vec::new();
        for output in outputs.iter() {
            let outpoint = output.outpoint;
            // Determine spentness from Bitcoin Core's UTXO set rather than the
            // esplora indexer — see `BitcoinRpc::is_output_unspent`.
            if self
                .bitcoin_rpc
                .is_output_unspent(&outpoint.txid, outpoint.vout)
            {
                utxos.push(*output);
            } else {
                utxos.push(ExplorerUtxo {
                    is_spent: true,
                    ..*output
                })
            }
        }

        utxos
    }
}

impl Blockchain for Regtest {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        Ok(self.find_outpoints_blocking(address))
    }

    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        // Query the node directly rather than the esplora indexer, which lags
        // the chain on regtest and would intermittently fail to return a
        // freshly-mined commitment TX (e.g. when building unilateral exit
        // trees).
        Ok(self.bitcoin_rpc.get_raw_transaction(txid))
    }

    async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        Ok(TxStatus {
            confirmed_at: self.bitcoin_rpc.get_tx_blocktime(txid),
        })
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .unwrap();

        Ok(SpendStatus {
            spend_txid: status.as_ref().and_then(|s| s.txid),
        })
    }

    // TODO: Make sure we return a proper error here, so that we can retry if we encounter a
    // `bad-txns-inputs-missingorspent` error.
    async fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
        self.esplora_client.broadcast(tx).unwrap();

        Ok(())
    }

    async fn get_fee_rate(&self) -> Result<f64, Error> {
        Ok(1.0)
    }

    async fn broadcast_package(&self, txs: &[&Transaction]) -> Result<(), Error> {
        let txs_hex = txs
            .iter()
            .map(bitcoin::consensus::encode::serialize_hex)
            .collect::<Vec<_>>();

        self.bitcoin_rpc.submit_package(txs_hex).await
    }
}

#[allow(unused)]
pub async fn set_up_client(
    _name: String,
    regtest: Arc<Regtest>,
    secp: Secp256k1<All>,
) -> (Client<Regtest, Wallet, InMemorySwapStorage>, Arc<Wallet>) {
    let mut rng = thread_rng();

    let sk = SecretKey::new(&mut rng);
    let kp = Keypair::from_secret_key(&secp, &sk);

    let network = Network::Regtest;

    let wallet = Wallet::new(kp, network, "http://localhost:3000/api").unwrap();
    let wallet = Arc::new(wallet);

    let seed: [u8; 32] = rng.r#gen();
    let xpriv = Xpriv::new_master(network, &seed).unwrap();

    let client = OfflineClient::with_bip32(
        OfflineClientConfig {
            ark_server_url: "http://localhost:7070".to_string(),
            boltz_url: "http://localhost:9069".to_string(),
            ..Default::default()
        },
        xpriv,
        None,
        regtest,
        wallet.clone(),
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .unwrap();

    (client, wallet)
}

#[allow(unused)]
pub async fn set_up_client_with_delegator(
    _name: String,
    regtest: Arc<Regtest>,
    secp: Secp256k1<All>,
    delegator_pk: XOnlyPublicKey,
) -> (Client<Regtest, Wallet, InMemorySwapStorage>, Arc<Wallet>) {
    let mut rng = thread_rng();

    let sk = SecretKey::new(&mut rng);
    let kp = Keypair::from_secret_key(&secp, &sk);

    let network = Network::Regtest;

    let wallet = Wallet::new(kp, network, "http://localhost:3000/api").unwrap();
    let wallet = Arc::new(wallet);

    let seed: [u8; 32] = rng.r#gen();
    let xpriv = Xpriv::new_master(network, &seed).unwrap();

    let config = OfflineClientConfig {
        ark_server_url: "http://localhost:7070".to_string(),
        boltz_url: "http://localhost:9069".to_string(),
        delegator_pk: Some(delegator_pk),
        historical_delegator_pks: vec![delegator_pk],
        ..Default::default()
    };

    let client = OfflineClient::with_bip32(
        config,
        xpriv,
        None,
        regtest,
        wallet.clone(),
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .unwrap();

    (client, wallet)
}

/// Wait until the client's offchain balance matches the specified targets.
///
/// Usage:
/// ```ignore
/// // Wait for confirmed balance only
/// wait_until_balance!(&client, confirmed: Amount::from_sat(1000));
///
/// // Wait for confirmed and pre_confirmed
/// wait_until_balance!(&client, confirmed: Amount::from_sat(1000), pre_confirmed: Amount::ZERO);
///
/// // Wait for all three
/// wait_until_balance!(&client, confirmed: Amount::from_sat(1000), pre_confirmed: Amount::ZERO, recoverable: Amount::ZERO);
///
/// // Any combination works
/// wait_until_balance!(&client, recoverable: Amount::from_sat(500));
/// ```
#[allow(unused)]
macro_rules! wait_until_balance {
    ($client:expr, $($field:ident : $target:expr),+ $(,)?) => {{
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let offchain_balance = $client.offchain_balance().await.expect("failed to get offchain balance");

                tracing::debug!(
                    ?offchain_balance,
                    $(
                        $field = %$target,
                    )+
                    "Waiting for balance to match targets"
                );

                let matches = true
                    $(
                        && wait_until_balance!(@check offchain_balance, $field, $target)
                    )+;

                if matches {
                    return;
                }

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
        .await
        .expect("timed out waiting for balance");
    }};

    (@check $balance:ident, confirmed, $target:expr) => {
        $balance.confirmed() == $target
    };
    (@check $balance:ident, pre_confirmed, $target:expr) => {
        $balance.pre_confirmed() == $target
    };
    (@check $balance:ident, recoverable, $target:expr) => {
        $balance.recoverable() == $target
    };
    (@check $balance:ident, pending_recovery, $target:expr) => {
        $balance.pending_recovery() == $target
    };
}

#[allow(unused)]
pub(crate) use wait_until_balance;

/// Set up a client with a specific seed for reproducible key derivation.
///
/// This is useful for testing key discovery, where we need to recreate a client
/// with the same keys.
#[allow(unused)]
pub async fn set_up_client_with_seed(
    name: String,
    regtest: Arc<Regtest>,
    secp: Secp256k1<All>,
    seed: [u8; 32],
) -> (Client<Regtest, Wallet, InMemorySwapStorage>, Arc<Wallet>) {
    set_up_client_with_seed_and_server_info_ttl(
        name,
        regtest,
        secp,
        seed,
        ark_client::DEFAULT_SERVER_INFO_TTL,
    )
    .await
}

/// Set up a client with a specific seed and server-info TTL.
#[allow(unused)]
pub async fn set_up_client_with_seed_and_server_info_ttl(
    _name: String,
    regtest: Arc<Regtest>,
    secp: Secp256k1<All>,
    seed: [u8; 32],
    server_info_ttl: Duration,
) -> (Client<Regtest, Wallet, InMemorySwapStorage>, Arc<Wallet>) {
    let mut rng = thread_rng();

    let sk = SecretKey::new(&mut rng);
    let kp = Keypair::from_secret_key(&secp, &sk);

    let network = Network::Regtest;

    let wallet = Wallet::new(kp, network, "http://localhost:3000/api").unwrap();
    let wallet = Arc::new(wallet);

    let xpriv = Xpriv::new_master(network, &seed).unwrap();

    let client = OfflineClient::with_bip32(
        OfflineClientConfig {
            ark_server_url: "http://localhost:7070".to_string(),
            boltz_url: "http://localhost:9069".to_string(),
            server_info_ttl,
            ..Default::default()
        },
        xpriv,
        None,
        regtest,
        wallet.clone(),
        Arc::new(InMemorySwapStorage::default()),
    )
    .connect_with_retries(5)
    .await
    .unwrap();

    (client, wallet)
}

#[derive(serde::Deserialize)]
struct LnAddInvoiceResponse {
    payment_request: String,
}

#[allow(unused)]
pub async fn create_lnd_invoice(amount: Amount) -> Bolt11Invoice {
    create_lnd_invoice_with_expiry(amount, None).await
}

#[allow(unused)]
pub async fn create_lnd_invoice_with_expiry(
    amount: Amount,
    expiry_secs: Option<u64>,
) -> Bolt11Invoice {
    let amount = amount.to_sat().to_string();
    let expiry = expiry_secs.map(|expiry| expiry.to_string());
    let mut args = vec![
        "exec",
        "lnd",
        "lncli",
        "--network=regtest",
        "addinvoice",
        "--amt",
        &amount,
    ];
    if let Some(expiry) = expiry.as_ref() {
        args.extend(["--expiry", expiry]);
    }

    let output = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
        .expect("failed to run lncli addinvoice");

    assert!(
        output.status.success(),
        "failed to create LND invoice: {}",
        format_command_output(&output)
    );

    let response: LnAddInvoiceResponse = serde_json::from_slice(&output.stdout)
        .expect("lncli addinvoice should return JSON with payment_request");
    response
        .payment_request
        .parse()
        .expect("lncli should return a valid BOLT11 invoice")
}

#[allow(unused)]
pub fn start_lnd_payment(invoice: &str) -> JoinHandle<std::io::Result<Output>> {
    let invoice = invoice.to_string();
    tokio::spawn(async move {
        tokio::process::Command::new("docker")
            .args([
                "exec",
                "lnd",
                "lncli",
                "--network=regtest",
                "payinvoice",
                "--force",
                &invoice,
            ])
            .output()
            .await
    })
}

#[allow(unused)]
pub async fn wait_for_lnd_payment(payment: JoinHandle<std::io::Result<Output>>) {
    let output = tokio::time::timeout(Duration::from_secs(30), payment)
        .await
        .expect("LN payment did not complete")
        .expect("lncli payinvoice task panicked")
        .expect("failed to wait for lncli payinvoice");

    assert!(
        output.status.success(),
        "failed to pay invoice: {}",
        format_command_output(&output)
    );
}

#[allow(unused)]
pub fn format_command_output(output: &Output) -> String {
    format!(
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn init_tracing() {
    static TRACING_TEST_SUBSCRIBER: Once = Once::new();

    TRACING_TEST_SUBSCRIBER.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                "debug,\
                 bdk=info,\
                 tower=info,\
                 hyper_util=info,\
                 hyper=info,\
                 h2=warn",
            )
            .with_test_writer()
            .init()
    })
}
