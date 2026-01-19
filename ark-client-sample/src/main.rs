#![allow(clippy::print_stdout)]
#![allow(clippy::large_enum_variant)]

mod common;

use crate::common::InMemoryDb;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use ark_bdk_wallet::Wallet;
use ark_client::lightning_invoice::Bolt11Invoice;
use ark_client::Blockchain;
use ark_client::Error;
use ark_client::OfflineClient;
use ark_client::SpendStatus;
use ark_client::SqliteSwapStorage;
use ark_client::StaticKeyProvider;
use ark_client::SwapAmount;
use ark_client::TxStatus;
use ark_core::boarding_output::list_boarding_outpoints;
use ark_core::history;
use ark_core::server::SubscriptionResponse;
use ark_core::ArkAddress;
use ark_core::BoardingOutput;
use ark_core::ExplorerUtxo;
use bitcoin::address::NetworkUnchecked;
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
use serde::Serialize;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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
    /// Send coins to an Ark address using specific VTXOs.
    SendToArkAddressWithVtxos {
        /// Comma-separated VTXO outpoints to use (format: txid:vout).
        #[arg(long)]
        vtxos: String,
        /// Where to send the coins to.
        address: ArkAddressCli,
        /// How many sats to send.
        amount: u64,
    },
    /// Transform boarding outputs and VTXOs into fresh, confirmed VTXOs.
    Settle,
    /// Subscribe to notifications for an Ark address.
    Subscribe {
        /// The Ark address to subscribe to.
        address: ArkAddressCli,
    },
    /// Send on-chain to address
    SendOnchain {
        /// Where to send the funds to
        address: Address<NetworkUnchecked>,
        /// How many sats to send.
        amount: u64,
    },
    /// Send on-chain to address using specific VTXOs.
    SendOnchainWithVtxos {
        /// Comma-separated VTXO outpoints to use (format: txid:vout).
        #[arg(long)]
        vtxos: String,
        /// Where to send the funds to.
        address: Address<NetworkUnchecked>,
        /// How many sats to send.
        amount: u64,
    },
    /// Generate a BOLT11 invoice to receive payment via a Boltz reverse submarine swap.
    LightningInvoice {
        /// How many sats to receive.
        amount: u64,
    },
    /// Pay a BOLT11 invoice via a Boltz submarine swap.
    PayInvoice {
        /// A BOLT11 invoice.
        invoice: String,
    },
    /// Attempt to refund a past swap collaboratively.
    RefundSwap { swap_id: String },
    /// Attempt to refund a past swap without the receiver's signature.
    RefundSwapWithoutReceiver { swap_id: String },
    /// List all VTXOs and boarding outputs sorted by expiry, then amount.
    ListVtxos,
    /// Settle specific VTXOs and/or boarding outputs by outpoint.
    SettleVtxos {
        /// VTXO outpoints to settle (format: txid:vout, comma-separated).
        #[arg(long)]
        vtxos: Option<String>,
        /// Boarding output outpoints to settle (format: txid:vout, comma-separated).
        #[arg(long)]
        boarding: Option<String>,
    },
    /// Estimate fees for sending (onchain for Bitcoin address, offchain for Ark address).
    EstimateFees {
        /// Where to send the funds to (Bitcoin address or Ark address).
        address: String,
        /// How many sats to send.
        amount: Option<u64>,
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
    swap_storage_path: String,
    boltz_url: String,
}

#[derive(Serialize)]
struct VtxoEntry {
    outpoint: String,
    amount_sats: u64,
    created_at: String,
    expires_at: String,
    status: String,
}

#[derive(Serialize)]
struct BoardingEntry {
    outpoint: String,
    amount_sats: u64,
    confirmation_time: Option<String>,
    status: String,
}

#[derive(Serialize)]
struct ListVtxosOutput {
    vtxos: Vec<VtxoEntry>,
    boarding_outputs: Vec<BoardingEntry>,
}

fn format_timestamp(unix_secs: i64) -> Result<String> {
    let ts = Timestamp::from_second(unix_secs)?;
    Ok(ts.to_string())
}

#[tokio::main]
#[allow(clippy::unwrap_in_result)]
async fn main() -> Result<()> {
    init_tracing();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("failed to install crypto providers"))?;

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

    let storage = Arc::new(
        SqliteSwapStorage::new(&config.swap_storage_path)
            .await
            .map_err(|e| anyhow!(e))?,
    );
    let client = OfflineClient::<_, _, _, StaticKeyProvider>::new_with_keypair(
        "sample-client".to_string(),
        kp,
        esplora_client.clone(),
        wallet,
        config.ark_server_url,
        storage,
        config.boltz_url,
        Duration::from_secs(30),
    )
    .connect()
    .await
    .map_err(|e| anyhow!(e))?;

    match &cli.command {
        Commands::Balance => {
            let offchain_balance = client.offchain_balance().await.map_err(|e| anyhow!(e))?;
            let boarding_balance = client.boarding_balance().await.map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "offchain_confirmed": offchain_balance.confirmed(),
                    "offchain_pre_confirmed": offchain_balance.pre_confirmed(),
                    "boarding_spendable": boarding_balance.spendable(),
                    "boarding_expired": boarding_balance.expired(),
                    "boarding_pending": boarding_balance.pending(),
                })
            );
        }
        Commands::TransactionHistory => {
            let tx_history = client.transaction_history().await.map_err(|e| anyhow!(e))?;
            if tx_history.is_empty() {
                tracing::info!("No transactions found");
            }
            for tx in tx_history.iter().rev() {
                tracing::info!("{}\n", pretty_print_transaction(tx)?);
            }
        }
        Commands::BoardingAddress => {
            let boarding_address = client.get_boarding_address().map_err(|e| anyhow!(e))?;
            println!(
                "{}",
                serde_json::json!({"address": boarding_address.to_string()})
            );
        }
        Commands::OffchainAddress => {
            let (address, _) = client.get_offchain_address().map_err(|e| anyhow!(e))?;
            let address = address.encode();
            println!("{}", serde_json::json!({"address": address}));
        }
        Commands::Settle => {
            let mut rng = thread_rng();
            // we need to call this because how our wallet works
            let _ = client.get_boarding_address();

            let maybe_batch_tx = client.settle(&mut rng).await.map_err(|e| anyhow!(e))?;
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
        Commands::SendToArkAddressWithVtxos {
            vtxos,
            address,
            amount,
        } => {
            // Parse comma-separated VTXO outpoints
            let vtxo_outpoints: Vec<OutPoint> = vtxos
                .split(',')
                .map(|op| {
                    OutPoint::from_str(op.trim()).with_context(|| format!("invalid outpoint: {op}"))
                })
                .collect::<Result<Vec<_>>>()?;

            let txid = client
                .send_vtxo_selection(&vtxo_outpoints, address.0, Amount::from_sat(*amount))
                .await
                .map_err(|e| anyhow!(e))?;
            tracing::info!(
                "Sent to address {} amount {} in txid {}",
                address.0,
                amount,
                txid
            );
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
                    Ok(SubscriptionResponse::Event(e)) => {
                        if let Some(psbt) = e.tx {
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
                    Ok(SubscriptionResponse::Heartbeat) => {}
                    Err(e) => {
                        tracing::error!("Error receiving subscription response: {e}");
                        break;
                    }
                }
            }

            println!("Subscription stream ended");
        }
        Commands::SendOnchain { address, amount } => {
            let network = client.server_info.network;
            let checked_address = address.clone().require_network(network)?;

            let mut rng = thread_rng();
            let txid = client
                .collaborative_redeem(&mut rng, checked_address.clone(), Amount::from_sat(*amount))
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                address = checked_address.to_string(),
                amount = amount.to_string(),
                txid = txid.to_string(),
                "Sent funds on-chain"
            );
        }
        Commands::SendOnchainWithVtxos {
            vtxos,
            address,
            amount,
        } => {
            // Parse comma-separated VTXO outpoints
            let vtxo_outpoints: Vec<OutPoint> = vtxos
                .split(',')
                .map(|op| {
                    OutPoint::from_str(op.trim()).with_context(|| format!("invalid outpoint: {op}"))
                })
                .collect::<Result<Vec<_>>>()?;

            let network = client.server_info.network;
            let checked_address = address.clone().require_network(network)?;

            let mut rng = thread_rng();
            let txid = client
                .collaborative_redeem_vtxo_selection(
                    &mut rng,
                    vtxo_outpoints.into_iter(),
                    checked_address.clone(),
                    Amount::from_sat(*amount),
                )
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                address = checked_address.to_string(),
                amount = amount.to_string(),
                txid = txid.to_string(),
                "Sent funds on-chain using selected VTXOs"
            );
        }
        Commands::LightningInvoice { amount } => {
            let invoice_amount = SwapAmount::invoice(Amount::from_sat(*amount));
            let res = client
                .get_ln_invoice(invoice_amount, None)
                .await
                .map_err(|e| anyhow!(e))?;

            let invoice = res.invoice.to_string();
            let swap_id = res.swap_id;

            tracing::info!(invoice, swap_id, "Lightning invoice");

            client
                .wait_for_vhtlc(&swap_id)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(invoice, swap_id, "Lightning invoice paid");
        }
        Commands::PayInvoice { invoice } => {
            let invoice = Bolt11Invoice::from_str(invoice)
                .map_err(|e| anyhow!("failed to parse BOLT11 invoice: {e}"))?;

            let result = client
                .pay_ln_invoice(invoice)
                .await
                .map_err(|e| anyhow!(e))?;

            let swap_id = result.swap_id;

            tracing::info!(swap_id, "Payment sent, waiting for finalization");

            client
                .wait_for_invoice_paid(swap_id.as_str())
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(swap_id, "Payment made");
        }
        Commands::RefundSwap { swap_id } => {
            let txid = client
                .refund_vhtlc(swap_id.as_str())
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(?txid, swap_id, "Swap refunded");
        }
        Commands::RefundSwapWithoutReceiver { swap_id } => {
            let txid = client
                .refund_expired_vhtlc(swap_id.as_str())
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(?txid, swap_id, "Swap refunded");
        }
        Commands::ListVtxos => {
            // Get VTXOs
            let (vtxo_list, _) = client.list_vtxos().await.map_err(|e| anyhow!(e))?;

            let mut vtxo_entries: Vec<VtxoEntry> = Vec::new();

            // Collect pre-confirmed VTXOs
            for v in vtxo_list.pre_confirmed() {
                vtxo_entries.push(VtxoEntry {
                    outpoint: v.outpoint.to_string(),
                    amount_sats: v.amount.to_sat(),
                    created_at: format_timestamp(v.created_at)?,
                    expires_at: format_timestamp(v.expires_at)?,
                    status: "pre_confirmed".to_string(),
                });
            }

            // Collect confirmed VTXOs
            for v in vtxo_list.confirmed() {
                vtxo_entries.push(VtxoEntry {
                    outpoint: v.outpoint.to_string(),
                    amount_sats: v.amount.to_sat(),
                    created_at: format_timestamp(v.created_at)?,
                    expires_at: format_timestamp(v.expires_at)?,
                    status: "confirmed".to_string(),
                });
            }

            // Collect expired VTXOs
            for v in vtxo_list.expired() {
                vtxo_entries.push(VtxoEntry {
                    outpoint: v.outpoint.to_string(),
                    amount_sats: v.amount.to_sat(),
                    created_at: format_timestamp(v.created_at)?,
                    expires_at: format_timestamp(v.expires_at)?,
                    status: "expired".to_string(),
                });
            }

            // Collect recoverable VTXOs
            for v in vtxo_list.recoverable() {
                vtxo_entries.push(VtxoEntry {
                    outpoint: v.outpoint.to_string(),
                    amount_sats: v.amount.to_sat(),
                    created_at: format_timestamp(v.created_at)?,
                    expires_at: format_timestamp(v.expires_at)?,
                    status: "recoverable".to_string(),
                });
            }

            // Sort by expiry (soonest first), then by amount (largest first)
            vtxo_entries.sort_by(|a, b| {
                a.expires_at
                    .cmp(&b.expires_at)
                    .then_with(|| b.amount_sats.cmp(&a.amount_sats))
            });

            // Get boarding outputs with proper status categorization (expired vs spendable)
            let boarding_outputs = vec![BoardingOutput::new(
                &Secp256k1::new(),
                client.server_info.signer_pk.into(),
                kp.x_only_public_key().0,
                client.server_info.boarding_exit_delay,
                client.server_info.network,
            )
            .map_err(|e| anyhow!(e))?];

            let esplora = esplora_client.clone();
            let find_outpoints = |address: &Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
                futures::executor::block_on(async {
                    esplora
                        .find_outpoints(address)
                        .await
                        .map_err(|e| ark_core::Error::ad_hoc(format!("{e}")))
                })
            };

            let outpoints = list_boarding_outpoints(find_outpoints, &boarding_outputs)
                .map_err(|e| anyhow!("failed to list boarding outpoints: {e}"))?;

            let mut boarding_entries: Vec<BoardingEntry> = Vec::new();

            // Add spendable (confirmed, not expired) boarding outputs
            for (outpoint, amount, _) in &outpoints.spendable {
                boarding_entries.push(BoardingEntry {
                    outpoint: outpoint.to_string(),
                    amount_sats: amount.to_sat(),
                    confirmation_time: None,
                    status: "spendable".to_string(),
                });
            }

            // Add expired boarding outputs (CSV timeout passed, refundable)
            for (outpoint, amount, _) in &outpoints.expired {
                boarding_entries.push(BoardingEntry {
                    outpoint: outpoint.to_string(),
                    amount_sats: amount.to_sat(),
                    confirmation_time: None,
                    status: "expired".to_string(),
                });
            }

            // Add pending (unconfirmed) boarding outputs
            for (outpoint, amount, _) in &outpoints.pending {
                boarding_entries.push(BoardingEntry {
                    outpoint: outpoint.to_string(),
                    amount_sats: amount.to_sat(),
                    confirmation_time: None,
                    status: "pending".to_string(),
                });
            }

            // Sort boarding by status (expired first so user knows to refund), then amount
            boarding_entries.sort_by(|a, b| {
                let status_order = |s: &str| match s {
                    "expired" => 0,
                    "spendable" => 1,
                    "pending" => 2,
                    _ => 3,
                };
                status_order(&a.status)
                    .cmp(&status_order(&b.status))
                    .then_with(|| b.amount_sats.cmp(&a.amount_sats))
            });

            let output = ListVtxosOutput {
                vtxos: vtxo_entries,
                boarding_outputs: boarding_entries,
            };

            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Commands::SettleVtxos { vtxos, boarding } => {
            let mut rng = thread_rng();

            // Parse VTXO outpoints
            let vtxo_outpoints: Vec<OutPoint> = match vtxos {
                Some(s) if !s.is_empty() => s
                    .split(',')
                    .map(|op| {
                        OutPoint::from_str(op.trim())
                            .with_context(|| format!("invalid outpoint: {op}"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => Vec::new(),
            };

            // Parse boarding outpoints
            let boarding_outpoints: Vec<OutPoint> = match boarding {
                Some(s) if !s.is_empty() => s
                    .split(',')
                    .map(|op| {
                        OutPoint::from_str(op.trim())
                            .with_context(|| format!("invalid outpoint: {op}"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => Vec::new(),
            };

            if vtxo_outpoints.is_empty() && boarding_outpoints.is_empty() {
                let output = serde_json::json!({
                    "commitment_txid": null,
                    "message": "No outpoints specified"
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
                return Ok(());
            }

            // We need to call this because of how our wallet works
            let _ = client.get_boarding_address();

            let maybe_batch_tx = client
                .settle_vtxos(&mut rng, &vtxo_outpoints, &boarding_outpoints)
                .await
                .map_err(|e| anyhow!(e))?;

            let output = match maybe_batch_tx {
                None => serde_json::json!({
                    "commitment_txid": null,
                    "message": "No matching inputs to settle"
                }),
                Some(txid) => serde_json::json!({
                    "commitment_txid": txid.to_string()
                }),
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Commands::EstimateFees { address, amount } => {
            let network = client.server_info.network;
            let mut rng = thread_rng();

            // Try parsing as ArkAddress first, then as Bitcoin address
            if let Ok(ark_address) = ArkAddress::decode(address) {
                let fees = client
                    .estimate_batch_fees(&mut rng, ark_address)
                    .await
                    .map_err(|e| anyhow!(e))?;

                let output = serde_json::json!({
                    "address": ark_address.encode(),
                    "address_type": "ark",
                    "estimated_fee_sats": fees.to_sat()
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                let amount = match amount {
                    None => {
                        bail!("Amount is required for Bitcoin address fee estimation")
                    }
                    Some(sats) => Amount::from_sat(*sats),
                };

                let bitcoin_address: Address<NetworkUnchecked> = address.parse()?;
                let checked_address = bitcoin_address.require_network(network)?;

                let fees = client
                    .estimate_onchain_fees(&mut rng, checked_address.clone(), amount)
                    .await
                    .map_err(|e| anyhow!(e))?;

                let output = serde_json::json!({
                    "address": checked_address.to_string(),
                    "address_type": "bitcoin",
                    "amount_sats": amount,
                    "estimated_fee_sats": fees.to_sat()
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
    }

    Ok(())
}

pub struct EsploraClient {
    esplora_client: esplora_client::AsyncClient,
}

impl Blockchain for EsploraClient {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
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
                    .map(|(i, v)| ExplorerUtxo {
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
                    utxos.push(ExplorerUtxo {
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

    async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        let info = self
            .esplora_client
            .get_tx_info(txid)
            .await
            .map_err(Error::consumer)?;

        Ok(TxStatus {
            confirmed_at: info.and_then(|s| s.status.block_time.map(|t| t as i64)),
        })
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .await
            .map_err(Error::consumer)?;

        Ok(SpendStatus {
            spend_txid: status.as_ref().and_then(|s| s.txid),
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
        history::Transaction::Offboard {
            commitment_txid,
            amount,
            confirmed_at,
        } => {
            let time = match confirmed_at {
                Some(t) => format!("{}", Timestamp::from_second(*t)?),
                None => "Pending confirmation".to_string(),
            };

            format!(
                "Type: Offboard\n\
                 Commitment TXID: {commitment_txid}\n\
                 Status: Sent (onchain)\n\
                 Amount: {amount}\n\
                 Time: {time}"
            )
        }
    };

    Ok(print_str)
}

pub fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "debug,\
                 tower=info,\
                 hyper_util=info,\
                 hyper=info,\
                 h2=warn,\
                 reqwest=info,\
                 ark_core=info,\
                 rustls=info,\
                 sqlx::query=info"
                    .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init()
}
