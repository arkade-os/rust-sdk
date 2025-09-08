#![allow(clippy::print_stdout)]
#![allow(clippy::large_enum_variant)]

mod common;

use crate::common::InMemoryDb;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use ark_bdk_wallet::Wallet;
use ark_client::Blockchain;
use ark_client::Error;
use ark_client::OfflineClient;
use ark_core::history;
use ark_core::ArkAddress;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Denomination;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::Transaction;
use bitcoin::Txid;
use clap::Parser;
use clap::Subcommand;
use esplora_client::OutputStatus;
use futures::StreamExt;
use jiff::Timestamp;
use rand::thread_rng;
use serde::Deserialize;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "ark-sample")]
#[command(about = "An Ark client in your terminal")]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "ark.config.toml")]
    config: String,

    /// Path to the seed file.
    #[arg(short, long, default_value = "ark.seed")]
    seed: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show the balance.
    Balance,
    /// Show the transaction history.
    TransactionHistory,
    /// Generate a boarding address.
    BoardingAddress,
    /// Generate an Ark address.
    OffchainAddress,
    /// Send coins to one or multiple Ark addresses.
    SendToArkAddresses {
        /// Where to send the coins to.
        addresses_and_amounts: AddressesAndAmounts,
    },
    /// Transform boarding outputs and VTXOs into fresh, confirmed VTXOs.
    Settle,
    /// Subscribe to notifications for an Ark address.
    Subscribe {
        /// The Ark address to subscribe to.
        address: ArkAddressCli,
    },
}

#[derive(Clone)]
struct ArkAddressCli(ArkAddress);

impl FromStr for ArkAddressCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let address = ArkAddress::decode(s)?;

        Ok(Self(address))
    }
}
#[derive(Clone)]
struct AddressesAndAmounts(Vec<(ArkAddress, Amount)>);

impl FromStr for AddressesAndAmounts {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = input.split(',').collect();

        if parts.len() % 2 != 0 {
            bail!("invalid input: expected comma-separated pairs of <address,amount in sats>");
        }

        let mut addresses_and_amounts = Vec::with_capacity(parts.len() / 2);
        for pair in parts.chunks(2) {
            let addr_raw = pair[0];
            let amt_raw = pair[1];
            let addr = ArkAddress::decode(addr_raw)
                .with_context(|| format!("failed to decode Ark address: {addr_raw}"))?;
            let amount = Amount::from_str_in(amt_raw, Denomination::Satoshi)
                .with_context(|| format!("failed to parse amount (sats): {amt_raw}"))?;
            addresses_and_amounts.push((addr, amount));
        }

        Ok(Self(addresses_and_amounts))
    }
}

#[derive(Deserialize)]
struct Config {
    ark_server_url: String,
    esplora_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("to be able to install crypto providers");

    let cli = Cli::parse();
    let secp = Secp256k1::new();

    let seed = fs::read_to_string(cli.seed)?;
    let sk = SecretKey::from_str(&seed)?;
    let kp = sk.keypair(&secp);

    let config = fs::read_to_string(cli.config)?;
    let config: Config = toml::from_str(&config)?;

    let db = InMemoryDb::default();
    let wallet = Wallet::new(kp, secp, Network::Regtest, config.esplora_url.as_str(), db)?;
    let wallet = Arc::new(wallet);

    let esplora_client = EsploraClient::new(&config.esplora_url)?;
    let esplora_client = Arc::new(esplora_client);

    let client = OfflineClient::new(
        "sample-client".to_string(),
        kp,
        esplora_client.clone(),
        wallet,
        config.ark_server_url,
        Duration::from_secs(30),
    )
    .connect()
    .await
    .map_err(|e| anyhow!(e))?;

    let info = &client.server_info;

    tracing::info!(?info, "Connected to ark server");

    match &cli.command {
        Commands::Balance => {
            let off_chain_balance = client.offchain_balance().await.map_err(|e| anyhow!(e))?;
            let boarding_output = client.get_boarding_address().map_err(|e| anyhow!(e))?;
            let outpoints = esplora_client
                .find_outpoints(&boarding_output)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                "Offchain balance: spendable = {}, pending = {}",
                off_chain_balance.confirmed(),
                off_chain_balance.pending()
            );
            let (spent, unspent): (Vec<_>, Vec<_>) =
                outpoints.into_iter().partition(|u| u.is_spent);

            let spent_sum = spent.iter().map(|u| u.amount).sum::<Amount>();
            let unspent_sum = unspent.iter().map(|u| u.amount).sum::<Amount>();

            tracing::info!(
                "Onchain balance: spendable = {}, spent = {}",
                unspent_sum,
                spent_sum
            );
        }
        Commands::TransactionHistory => {
            let tx_history = client.transaction_history().await.map_err(|e| anyhow!(e))?;
            if tx_history.is_empty() {
                tracing::info!("No transactions found");
            }
            for tx in tx_history.iter() {
                tracing::info!("{}\n", pretty_print_transaction(tx)?);
            }
        }
        Commands::BoardingAddress => {
            let boarding_address = client.get_boarding_address().map_err(|e| anyhow!(e))?;
            tracing::info!("Send coins to this on-chain address: {boarding_address}");
        }
        Commands::OffchainAddress => {
            let (address, _) = client.get_offchain_address().map_err(|e| anyhow!(e))?;
            let address = address.encode();
            tracing::info!("Send VTXOs to this offchain address: {address}");
        }
        Commands::Settle => {
            let mut rng = thread_rng();
            // we need to call this because how our wallet works
            let _ = client.get_boarding_address();

            let maybe_batch_tx = client
                .settle(&mut rng, false)
                .await
                .map_err(|e| anyhow!(e))?;
            match maybe_batch_tx {
                None => {
                    tracing::info!("No batch transaction - maybe nothing to settle");
                }
                Some(txid) => {
                    tracing::info!("Successfully settled in {txid}");
                }
            }
        }
        Commands::SendToArkAddresses {
            addresses_and_amounts,
        } => {
            for (address, amount) in &addresses_and_amounts.0 {
                let txid = client
                    .send_vtxo(*address, *amount)
                    .await
                    .map_err(|e| anyhow!(e))?;
                tracing::info!("Sent to address {address} amount {amount} in txid {txid}")
            }
        }
        Commands::Subscribe { address } => {
            tracing::info!("Subscribing to address: {}", address.0);
            // First subscribe to the address to get a subscription ID
            let subscription_id = client
                .subscribe_to_scripts(vec![address.0], None)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!("Subscription ID: {subscription_id}",);

            // Now get the subscription stream
            let mut subscription_stream = client
                .get_subscription(subscription_id)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!("Listening for notifications... Press Ctrl+C to stop");

            // Process subscription responses as they come in
            while let Some(result) = subscription_stream.next().await {
                match result {
                    Ok(response) => {
                        if let Some(psbt) = response.tx {
                            let tx = &psbt.unsigned_tx;
                            let output = tx.output.to_vec().iter().find_map(|out| {
                                if out.script_pubkey == address.0.to_p2tr_script_pubkey() {
                                    Some(out.clone())
                                } else {
                                    None
                                }
                            });
                            match output {
                                None => {
                                    tracing::warn!(
                                        "Received subscription response did not include our address"
                                    );
                                }
                                Some(output) => {
                                    tracing::info!("Received subscription response:");
                                    tracing::info!("  TXID: {}", tx.compute_txid());
                                    tracing::info!("  Output Value: {:?}", output.value);
                                    tracing::info!("  Output Address: {:?}", address.0.encode());
                                }
                            }
                        } else {
                            tracing::warn!("No tx found");
                        };

                        tracing::info!("---");
                    }
                    Err(e) => {
                        tracing::error!("Error receiving subscription response: {e}");
                        break;
                    }
                }
            }

            println!("Subscription stream ended");
        }
    }

    Ok(())
}

pub struct EsploraClient {
    esplora_client: esplora_client::AsyncClient,
}

impl Blockchain for EsploraClient {
    async fn find_outpoints(
        &self,
        address: &Address,
    ) -> Result<Vec<ark_client::ExplorerUtxo>, Error> {
        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .await
            .map_err(Error::consumer)?;

        let outputs = txs
            .into_iter()
            .flat_map(|tx| {
                let txid = tx.txid;
                tx.vout
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.scriptpubkey == script_pubkey)
                    .map(|(i, v)| ark_client::ExplorerUtxo {
                        outpoint: OutPoint {
                            txid,
                            vout: i as u32,
                        },
                        amount: Amount::from_sat(v.value),
                        confirmation_blocktime: tx.status.block_time,
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
                .await
                .map_err(Error::consumer)?;

            match status {
                Some(OutputStatus { spent: false, .. }) | None => {
                    utxos.push(*output);
                }
                Some(OutputStatus { spent: true, .. }) => {
                    utxos.push(ark_client::ExplorerUtxo {
                        is_spent: true,
                        ..*output
                    });
                }
            }
        }

        Ok(utxos)
    }

    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        let option = self
            .esplora_client
            .get_tx(txid)
            .await
            .map_err(Error::consumer)?;
        Ok(option)
    }

    async fn get_output_status(
        &self,
        txid: &Txid,
        vout: u32,
    ) -> Result<ark_client::SpendStatus, Error> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .await
            .map_err(Error::consumer)?;

        Ok(ark_client::SpendStatus {
            spend_txid: status.and_then(|s| s.txid),
        })
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
        self.esplora_client
            .broadcast(tx)
            .await
            .map_err(Error::consumer)?;
        Ok(())
    }

    async fn get_fee_rate(&self) -> Result<f64, Error> {
        Ok(1.0)
    }

    async fn broadcast_package(&self, _txs: &[&Transaction]) -> Result<(), Error> {
        unimplemented!("Not implemented yet");
    }
}

impl EsploraClient {
    pub fn new(url: &str) -> Result<Self> {
        let builder = esplora_client::Builder::new(url);
        let esplora_client = builder.build_async()?;

        Ok(Self { esplora_client })
    }
}

fn pretty_print_transaction(tx: &history::Transaction) -> Result<String> {
    let print_str = match tx {
        history::Transaction::Boarding {
            txid,
            amount,
            confirmed_at,
        } => {
            let time = match confirmed_at {
                Some(t) => format!("{}", Timestamp::from_second(*t)?),
                None => "Pending confirmation".to_string(),
            };

            format!(
                "Type: Boarding\n\
                 TXID: {txid}\n\
                 Status: Received\n\
                 Amount: {amount}\n\
                 Time: {time}"
            )
        }
        history::Transaction::Commitment {
            txid,
            amount,
            created_at,
        } => {
            let status = match amount.is_positive() {
                true => "Received",
                false => "Sent",
            };

            let amount = amount.abs();

            let time = Timestamp::from_second(*created_at)?;

            format!(
                "Type: Commitment\n\
                 TXID: {txid}\n\
                 Status: {status}\n\
                 Amount: {amount}\n\
                 Time: {time}"
            )
        }
        history::Transaction::Ark {
            txid,
            amount,
            is_settled,
            created_at,
        } => {
            let status = match amount.is_positive() {
                true => "Received",
                false => "Sent",
            };

            let settlement = match is_settled {
                true => "Confirmed",
                false => "Pending",
            };

            let amount = amount.abs();

            let time = Timestamp::from_second(*created_at)?;

            format!(
                "Type: Ark\n\
                 TXID: {txid}\n\
                 Status: {status}\n\
                 Settlement: {settlement}\n\
                 Amount: {amount}\n\
                 Time: {time}"
            )
        }
    };

    Ok(print_str)
}

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            "debug,\
             tower=info,\
             hyper_util=info,\
             hyper=info,\
             h2=warn,\
             reqwest=info,\
             ark_core=info,\
             rustls=info",
        )
        .init()
}
