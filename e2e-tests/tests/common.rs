#![allow(clippy::unwrap_used)]

use ark_bdk_wallet::Wallet;
use ark_client::error::Error;
use ark_client::wallet::Persistence;
use ark_client::Blockchain;
use ark_client::Client;
use ark_client::ExplorerUtxo;
use ark_client::OfflineClient;
use ark_client::SpendStatus;
use ark_core::BoardingOutput;
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
use regex::Regex;
use std::collections::HashMap;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Once;
use std::sync::RwLock;
use std::time::Duration;

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
                "Bitcoin RPC submitpackage failed: {response_text}"
            )));
        }

        // Parse JSON-RPC response to check for RPC-level errors
        let rpc_response: serde_json::Value = serde_json::from_str(&response_text)
            .map_err(|e| Error::wallet(format!("Failed to parse RPC response: {e}")))?;

        if let Some(error) = rpc_response.get("error") {
            return Err(Error::wallet(format!(
                "Bitcoin RPC submitpackage error: {error}"
            )));
        }

        tracing::debug!(
            "Successfully submitted package of {} transactions",
            txs.len()
        );
        Ok(())
    }
}

pub struct Nigiri {
    esplora_client: esplora_client::BlockingClient,
    /// By how much we _reduce_ the block time of outpoints. A lower block time indicates that an
    /// outpoint was confirmed longer ago.
    ///
    /// This can be used to ensure that certain outpoints are considered spendable, which is useful
    /// for testing scripts with opcodes such as `OP_CSV`.
    outpoint_blocktime_offset: RwLock<u64>,
    /// Bitcoin RPC client for package submission
    bitcoin_rpc: BitcoinRpc,
}

impl Nigiri {
    pub fn new() -> Self {
        let esplora_url = "http://localhost:30000";
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
            bitcoin_rpc,
        }
    }

    pub async fn faucet_fund(&self, address: &Address, amount: Amount) -> OutPoint {
        let res = Command::new("nigiri")
            .args(["faucet", &address.to_string(), &amount.to_btc().to_string()])
            .output()
            .unwrap();

        assert!(res.status.success());

        let text = String::from_utf8(res.stdout).unwrap();
        let re = Regex::new(r"txId: ([0-9a-fA-F]{64})").unwrap();

        let txid = match re.captures(&text) {
            Some(captures) => match captures.get(1) {
                Some(txid) => txid.as_str(),
                _ => panic!("Could not parse TXID"),
            },
            None => {
                panic!("Could not parse TXID");
            }
        };

        let txid: Txid = txid.parse().unwrap();

        let res = Command::new("nigiri")
            .args(["rpc", "getrawtransaction", &txid.to_string()])
            .output()
            .unwrap();

        let tx = String::from_utf8(res.stdout).unwrap();

        let tx = Vec::from_hex(tx.trim()).unwrap();
        let tx: Transaction = bitcoin::consensus::deserialize(&tx).unwrap();

        let (vout, _) = tx
            .output
            .iter()
            .enumerate()
            .find(|(_, o)| o.script_pubkey == address.script_pubkey())
            .unwrap();

        // Wait for output to be confirmed.
        tokio::time::sleep(Duration::from_secs(5)).await;

        OutPoint {
            txid,
            vout: vout as u32,
        }
    }

    #[allow(unused)]
    pub fn set_outpoint_blocktime_offset(&self, outpoint_blocktime_offset: u64) {
        let mut guard = self.outpoint_blocktime_offset.write().unwrap();
        *guard = outpoint_blocktime_offset;
    }

    #[allow(unused)]
    pub async fn mine(&self, n: u32) {
        for i in 0..n {
            self.faucet_fund(
                &Address::from_str("bcrt1q8frde3yn78tl9ecgq4anlz909jh0clefhucdur")
                    .unwrap()
                    .assume_checked(),
                Amount::from_sat(10_000),
            )
            .await;
        }

        tracing::debug!(n, "Mined blocks");
    }
}

impl Default for Nigiri {
    fn default() -> Self {
        Self::new()
    }
}

impl Blockchain for Nigiri {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .unwrap();

        let outputs = txs
            .into_iter()
            .flat_map(|tx| {
                let txid = tx.txid;

                let confirmation_blocktime = tx
                    .status
                    .block_time
                    .map(|t| t - *self.outpoint_blocktime_offset.read().unwrap());

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
                        // Assume the output is unspent until we dig deeper, further down.
                        is_spent: false,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut utxos = Vec::new();
        for output in outputs.iter() {
            let outpoint = output.outpoint;
            let status = self
                .esplora_client
                .get_output_status(&outpoint.txid, outpoint.vout as u64)
                .unwrap();

            match status {
                Some(esplora_client::OutputStatus { spent: false, .. }) | None => {
                    utxos.push(*output);
                }
                Some(esplora_client::OutputStatus { spent: true, .. }) => {
                    utxos.push(ExplorerUtxo {
                        is_spent: true,
                        ..*output
                    })
                }
            }
        }

        Ok(utxos)
    }

    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        let tx = self.esplora_client.get_tx(txid).unwrap();

        Ok(tx)
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .unwrap();

        Ok(SpendStatus {
            spend_txid: status.and_then(|s| s.txid),
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

#[derive(Default)]
pub struct InMemoryDb {
    boarding_outputs: RwLock<HashMap<BoardingOutput, SecretKey>>,
}

impl Persistence for InMemoryDb {
    fn save_boarding_output(
        &self,
        sk: SecretKey,
        boarding_output: BoardingOutput,
    ) -> Result<(), Error> {
        self.boarding_outputs
            .write()
            .unwrap()
            .insert(boarding_output, sk);

        Ok(())
    }

    fn load_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, Error> {
        Ok(self
            .boarding_outputs
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect())
    }

    fn sk_for_pk(&self, pk: &XOnlyPublicKey) -> Result<SecretKey, Error> {
        let maybe_sk = self
            .boarding_outputs
            .read()
            .unwrap()
            .iter()
            .find_map(|(b, sk)| if b.owner_pk() == *pk { Some(*sk) } else { None });
        let secret_key = maybe_sk.unwrap();
        Ok(secret_key)
    }
}

pub async fn set_up_client(
    name: String,
    nigiri: Arc<Nigiri>,
    secp: Secp256k1<All>,
) -> (Client<Nigiri, Wallet<InMemoryDb>>, Arc<Wallet<InMemoryDb>>) {
    let mut rng = thread_rng();

    let sk = SecretKey::new(&mut rng);
    let kp = Keypair::from_secret_key(&secp, &sk);

    let db = InMemoryDb::default();
    let wallet = Wallet::new(kp, secp, Network::Regtest, "http://localhost:3000", db).unwrap();
    let wallet = Arc::new(wallet);

    let client = OfflineClient::new(
        name,
        kp,
        nigiri,
        wallet.clone(),
        "http://localhost:7070".to_string(),
    )
    .connect()
    .await
    .unwrap();

    (client, wallet)
}

#[allow(unused)]
pub async fn wait_until_balance(
    client: &Client<Nigiri, Wallet<InMemoryDb>>,
    confirmed_target: Amount,
    pending_target: Amount,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let offchain_balance = client.offchain_balance().await.unwrap();

            tracing::debug!(
                ?offchain_balance,
                %confirmed_target,
                %pending_target,
                "Waiting for balance to match targets"
            );

            if offchain_balance.confirmed() == confirmed_target
                && offchain_balance.pending() == pending_target
            {
                return;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap();
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
