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
use ark_client::Bip32KeyProvider;
use ark_client::Blockchain;
use ark_client::ChainSwapAmount;
use ark_client::ChainSwapDirection;
use ark_client::Error;
use ark_client::KeyProvider;
use ark_client::OfflineClient;
use ark_client::SpendStatus;
use ark_client::SqliteSwapStorage;
use ark_client::StaticKeyProvider;
use ark_client::SwapAmount;
use ark_client::TxStatus;
use ark_core::asset::ControlAssetConfig;
use ark_core::history;
use ark_core::send::SendReceiver;
use ark_core::send::VtxoInput;
use ark_core::server::SubscriptionResponse;
use ark_core::ArkAddress;
use ark_core::ArkNote;
use ark_core::ExplorerUtxo;
use ark_grpc::test_utils as grpc_test_utils;
use bitcoin::address::NetworkUnchecked;
use bitcoin::bip32::Xpriv;
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

    /// Path to a BIP39 mnemonic file.
    #[arg(short, long)]
    mnemonic: Option<String>,

    /// Path to a hex-encoded secret key file.
    #[arg(short, long)]
    seed: Option<String>,

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
    Settle {
        /// ArkNote strings to include in the settlement (comma-separated).
        #[arg(long)]
        notes: Option<String>,
    },
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
    /// Create a chain swap (ARK <-> BTC) via Boltz.
    ChainSwap {
        /// Direction: "ark-to-btc" or "btc-to-ark".
        direction: String,
        /// Amount in sats.
        amount: u64,
        /// Target BTC address (required for ark-to-btc).
        #[arg(long)]
        address: Option<String>,
        /// Fee rate in sat/vB for the on-chain claim (default: 1.0).
        #[arg(long, default_value = "1.0")]
        fee_rate: f64,
    },
    /// Claim a chain swap after the server has locked funds.
    ClaimChainSwap {
        /// The Boltz swap ID.
        swap_id: String,
        /// Target BTC address (required for ark-to-btc claims).
        #[arg(long)]
        address: Option<String>,
        /// Fee rate in sat/vB for the on-chain claim (default: 1.0).
        #[arg(long, default_value = "1.0")]
        fee_rate: f64,
    },
    /// Check the status of a Boltz swap.
    SwapStatus {
        /// The Boltz swap ID.
        swap_id: String,
    },
    /// Refund a chain swap (reclaim locked funds after expiry).
    RefundChainSwap {
        /// The Boltz swap ID.
        swap_id: String,
        /// Target BTC address (required for btc-to-ark refunds).
        #[arg(long)]
        address: Option<String>,
        /// Fee rate in sat/vB for the on-chain refund (default: 1.0).
        #[arg(long, default_value = "1.0")]
        fee_rate: f64,
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
    /// List pending (submitted but not finalized) offchain transactions.
    ListPendingTxs,
    /// Continue and finalize any pending offchain transactions.
    ContinuePendingTxs,
    /// List pending (submitted but not finalized) VHTLC swap transactions.
    ListPendingSwapTxs,
    /// Continue and finalize a pending VHTLC swap transaction by Boltz swap ID.
    ContinuePendingSwapTxs {
        /// The Boltz swap ID of the pending VHTLC swap transaction to finalize.
        swap_id: String,
    },
    /// Submit an offchain tx WITHOUT finalizing (for testing pending tx recovery).
    SubmitOnly { address: ArkAddressCli, amount: u64 },
    /// Create ArkNotes via the admin API (regtest only).
    CreateNote {
        /// Amount in satoshis for each note.
        amount: u64,
        /// Number of notes to create (default: 1).
        #[arg(short, long, default_value = "1")]
        quantity: u32,
        /// Admin API URL (default: http://localhost:7071).
        #[arg(long, default_value = "http://localhost:7071")]
        admin_url: String,
    },
    /// Get information about an asset by its ID.
    GetAsset {
        /// The asset ID to look up.
        asset_id: String,
    },
    /// Send an asset to an Ark address (BTC amount is automatically set to dust).
    SendAssets {
        /// Destination Ark address.
        address: ArkAddressCli,
        /// The asset ID to send.
        asset_id: String,
        /// The amount of the asset to send.
        amount: u64,
    },
    /// Burn a specific amount of an asset.
    BurnAsset {
        /// The asset ID to burn.
        asset_id: String,
        /// The amount of the asset to burn.
        amount: u64,
    },
    /// Reissue additional units of an existing asset (requires control asset).
    ReissueAsset {
        /// The asset ID to reissue.
        asset_id: String,
        /// The amount of additional asset units to mint.
        amount: u64,
    },
    /// Issue a new asset.
    IssueAsset {
        /// Number of asset units to issue.
        amount: u64,
        /// Create a new control asset with this amount (enables reissuance).
        #[arg(long)]
        control_amount: Option<u64>,
        /// Use an existing control asset ID (format: txid:gidx).
        #[arg(long)]
        control_asset_id: Option<String>,
        /// Metadata key-value pairs (format: key=value, comma-separated).
        #[arg(long)]
        metadata: Option<String>,
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

#[derive(Serialize)]
struct PendingTxEntry {
    ark_txid: String,
    num_inputs: usize,
    num_outputs: usize,
    total_output_sats: u64,
}

#[derive(Serialize)]
struct ListPendingTxsOutput {
    pending_txs: Vec<PendingTxEntry>,
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

    // Handle CreateNote early - it doesn't need a wallet or config
    if let Commands::CreateNote {
        amount,
        quantity,
        admin_url,
    } = &cli.command
    {
        let notes = grpc_test_utils::create_notes_with_url(admin_url, *amount as u32, *quantity)
            .await
            .map_err(|e| anyhow!("failed to create notes: {e}"))?;

        let output: Vec<_> = notes
            .iter()
            .map(|note| {
                serde_json::json!({
                    "note": note.to_encoded_string(),
                    "value_sats": note.value().to_sat(),
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    let secp = Secp256k1::new();

    let config = fs::read_to_string(&cli.config)?;
    let config: Config = toml::from_str(&config)?;

    let esplora_client = EsploraClient::new(&config.esplora_url)?;
    let esplora_client = Arc::new(esplora_client);

    let storage = Arc::new(
        SqliteSwapStorage::new(&config.swap_storage_path)
            .await
            .map_err(|e| anyhow!(e))?,
    );

    match (cli.mnemonic, cli.seed) {
        (Some(_), Some(_)) => bail!("specify either --mnemonic or --seed, not both"),
        (None, None) => bail!("specify either --mnemonic or --seed"),
        (Some(mnemonic_path), None) => {
            let mnemonic_str = fs::read_to_string(mnemonic_path)?;
            let mnemonic = bip39::Mnemonic::parse_normalized(mnemonic_str.trim())
                .map_err(|e| anyhow!("invalid mnemonic: {e}"))?;
            let seed = mnemonic.to_seed("");
            let xpriv = Xpriv::new_master(Network::Regtest, &seed)?;

            let db = InMemoryDb::default();
            let wallet = Wallet::new_from_xpriv(
                xpriv,
                secp,
                Network::Regtest,
                config.esplora_url.as_str(),
                db,
            )?;
            let wallet = Arc::new(wallet);

            let client = OfflineClient::<_, _, _, Bip32KeyProvider>::new_with_bip32(
                "sample-client".to_string(),
                xpriv,
                None,
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

            run_command(cli.command, client, esplora_client).await?;
        }
        (None, Some(seed_path)) => {
            let seed = fs::read_to_string(seed_path)?;
            let sk = SecretKey::from_str(seed.trim())?;
            let kp = sk.keypair(&secp);

            let db = InMemoryDb::default();
            let wallet = Wallet::new(kp, secp, Network::Regtest, config.esplora_url.as_str(), db)?;
            let wallet = Arc::new(wallet);

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

            run_command(cli.command, client, esplora_client).await?;
        }
    }

    Ok(())
}

async fn run_command<K: KeyProvider>(
    command: Commands,
    client: ark_client::Client<EsploraClient, Wallet<InMemoryDb>, SqliteSwapStorage, K>,
    esplora_client: Arc<EsploraClient>,
) -> Result<()> {
    client.discover_keys(20).await.map_err(|e| anyhow!(e))?;

    match &command {
        Commands::Balance => {
            let offchain_balance = client.offchain_balance().await.map_err(|e| anyhow!(e))?;

            let boarding = {
                let boarding_output = client.get_boarding_address().map_err(|e| anyhow!(e))?;
                let outpoints = esplora_client
                    .find_outpoints(&boarding_output)
                    .await
                    .map_err(|e| anyhow!(e))?;

                let (_, unspent): (Vec<_>, Vec<_>) =
                    outpoints.into_iter().partition(|u| u.is_spent);

                unspent.iter().map(|u| u.amount).sum::<Amount>()
            };

            let mut balance_json = serde_json::json!({
                "offchain_confirmed": offchain_balance.confirmed(),
                "offchain_pre_confirmed": offchain_balance.pre_confirmed(),
                "recoverable": offchain_balance.recoverable(),
                "boarding": boarding,
            });

            if !offchain_balance.asset_balances().is_empty() {
                balance_json["assets"] = serde_json::to_value(offchain_balance.asset_balances())?;
            }

            println!("{}", balance_json);
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
        Commands::Settle { notes } => {
            let mut rng = thread_rng();
            // we need to call this because how our wallet works
            let _ = client.get_boarding_address();

            let maybe_batch_tx = match notes {
                Some(notes_str) => {
                    // Parse comma-separated ArkNote strings
                    let parsed_notes: Vec<ArkNote> = notes_str
                        .split(',')
                        .map(|s| {
                            ArkNote::from_string(s.trim())
                                .with_context(|| format!("invalid ArkNote: {s}"))
                        })
                        .collect::<Result<Vec<_>>>()?;

                    if parsed_notes.is_empty() {
                        bail!("No valid ArkNotes provided");
                    }

                    let total_note_value: u64 =
                        parsed_notes.iter().map(|n| n.value().to_sat()).sum();
                    tracing::info!(
                        num_notes = parsed_notes.len(),
                        total_value = total_note_value,
                        "Settling with ArkNotes"
                    );

                    client
                        .settle_with_notes(&mut rng, parsed_notes)
                        .await
                        .map_err(|e| anyhow!(e))?
                }
                None => client.settle(&mut rng).await.map_err(|e| anyhow!(e))?,
            };

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
                    .send(vec![SendReceiver::bitcoin(*address, *amount)])
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
                .send_selection(
                    &vtxo_outpoints,
                    vec![SendReceiver::bitcoin(address.0, Amount::from_sat(*amount))],
                )
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
        Commands::ChainSwap {
            direction,
            amount,
            address,
            fee_rate,
        } => {
            let direction = match direction.as_str() {
                "ark-to-btc" => ChainSwapDirection::ArkToBtc,
                "btc-to-ark" => ChainSwapDirection::BtcToArk,
                other => {
                    bail!("invalid direction '{other}', expected 'ark-to-btc' or 'btc-to-ark'")
                }
            };

            let amount = ChainSwapAmount::UserLock(Amount::from_sat(*amount));

            let result = client
                .create_chain_swap(direction.clone(), amount)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                swap_id = result.swap_id,
                user_lockup_address = %result.user_lockup_address,
                user_lockup_amount = %result.user_lockup_amount,
                server_lockup_amount = %result.server_lockup_amount,
                bip21 = result.bip21.as_deref().unwrap_or("n/a"),
                "Chain swap created — fund the user_lockup_address to proceed"
            );

            match direction {
                ChainSwapDirection::BtcToArk => {
                    // BtcToArk: user funds BTC on-chain, then claims Ark VHTLC
                    tracing::info!(swap_id = result.swap_id, "Waiting for server lockup...");

                    client
                        .wait_for_chain_swap_server_lockup(&result.swap_id)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(
                        swap_id = result.swap_id,
                        "Server locked ARK VHTLC, claiming..."
                    );

                    let txid = client
                        .claim_chain_swap(&result.swap_id)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(swap_id = result.swap_id, %txid, "Chain swap claimed (ARK)");
                }
                ChainSwapDirection::ArkToBtc => {
                    // ArkToBtc: fund Ark VHTLC, wait for server BTC lockup, claim BTC
                    let destination: Address = address
                        .as_deref()
                        .ok_or_else(|| anyhow!("--address is required for ark-to-btc"))?
                        .parse::<Address<NetworkUnchecked>>()
                        .map_err(|e| anyhow!("invalid BTC address: {e}"))?
                        .assume_checked();

                    let lockup_address = ArkAddress::decode(&result.user_lockup_address)
                        .map_err(|e| anyhow!("failed to parse ARK lockup address: {e}"))?;

                    tracing::info!(swap_id = result.swap_id, "Funding ARK VHTLC...");

                    let fund_txid = client
                        .send(vec![SendReceiver::bitcoin(
                            lockup_address,
                            result.user_lockup_amount,
                        )])
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(
                        swap_id = result.swap_id,
                        %fund_txid,
                        "Funded ARK VHTLC, waiting for server BTC lockup..."
                    );

                    client
                        .wait_for_chain_swap_server_lockup(&result.swap_id)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(
                        swap_id = result.swap_id,
                        "Server locked BTC, claiming on-chain..."
                    );

                    let txid = client
                        .claim_chain_swap_btc(&result.swap_id, destination, *fee_rate)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(swap_id = result.swap_id, %txid, "Chain swap claimed (BTC)");
                }
            }
        }
        Commands::ClaimChainSwap {
            swap_id,
            address,
            fee_rate,
        } => {
            // Try Ark VHTLC claim first; if it fails (wrong direction), try BTC claim.
            match client.claim_chain_swap(swap_id).await {
                Ok(txid) => {
                    tracing::info!(%txid, swap_id, "Chain swap claimed (ARK VHTLC)");
                }
                Err(_) => {
                    let destination: Address = address
                        .as_deref()
                        .ok_or_else(|| anyhow!("--address is required for ark-to-btc claims"))?
                        .parse::<Address<NetworkUnchecked>>()
                        .map_err(|e| anyhow!("invalid BTC address: {e}"))?
                        .assume_checked();

                    let txid = client
                        .claim_chain_swap_btc(swap_id, destination, *fee_rate)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(%txid, swap_id, "Chain swap claimed (on-chain BTC)");
                }
            }
        }
        Commands::SwapStatus { swap_id } => {
            let info = client
                .get_swap_status(swap_id.as_str())
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                swap_id,
                swap_type = %info.swap_type,
                status = ?info.status,
                "Swap status"
            );
        }
        Commands::RefundChainSwap {
            swap_id,
            address,
            fee_rate,
        } => {
            // Try the Ark VHTLC refund first (ArkToBtc direction).
            match client.refund_chain_swap(swap_id).await {
                Ok(txid) => {
                    tracing::info!(%txid, swap_id, "Chain swap refunded (ARK VHTLC)");
                }
                Err(ark_err) => {
                    // If no --address provided, it's an Ark refund that failed — report the error.
                    let Some(addr_str) = address.as_deref() else {
                        return Err(anyhow!(ark_err).context(
                            "Ark VHTLC refund failed (pass --address for on-chain BTC refund)",
                        ));
                    };

                    tracing::debug!(
                        "Ark VHTLC refund failed ({ark_err}), trying on-chain BTC refund"
                    );

                    let destination: Address = addr_str
                        .parse::<Address<NetworkUnchecked>>()
                        .map_err(|e| anyhow!("invalid BTC address: {e}"))?
                        .assume_checked();

                    let txid = client
                        .refund_chain_swap_btc(swap_id, destination, *fee_rate)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    tracing::info!(%txid, swap_id, "Chain swap refunded (on-chain BTC)");
                }
            }
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

            // Get boarding outputs
            let boarding_output = client.get_boarding_address().map_err(|e| anyhow!(e))?;
            let outpoints = esplora_client
                .find_outpoints(&boarding_output)
                .await
                .map_err(|e| anyhow!(e))?;

            let mut boarding_entries: Vec<BoardingEntry> = Vec::new();
            for o in outpoints {
                if !o.is_spent {
                    boarding_entries.push(BoardingEntry {
                        outpoint: o.outpoint.to_string(),
                        amount_sats: o.amount.to_sat(),
                        confirmation_time: o
                            .confirmation_blocktime
                            .map(|t| format_timestamp(t as i64))
                            .transpose()?,
                        status: if o.confirmation_blocktime.is_some() {
                            "confirmed".to_string()
                        } else {
                            "pending".to_string()
                        },
                    });
                }
            }

            // Sort boarding by confirmation time (earliest first), then amount (largest first)
            boarding_entries.sort_by(|a, b| {
                a.confirmation_time
                    .cmp(&b.confirmation_time)
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
        Commands::ListPendingTxs => {
            let pending_txs = client
                .list_pending_offchain_txs()
                .await
                .map_err(|e| anyhow!(e))?;

            let entries: Vec<PendingTxEntry> = pending_txs
                .iter()
                .map(|tx| {
                    let total_output_sats = tx
                        .signed_ark_tx
                        .unsigned_tx
                        .output
                        .iter()
                        .map(|o| o.value.to_sat())
                        .sum();

                    PendingTxEntry {
                        ark_txid: tx.ark_txid.to_string(),
                        num_inputs: tx.signed_ark_tx.unsigned_tx.input.len(),
                        num_outputs: tx.signed_ark_tx.unsigned_tx.output.len(),
                        total_output_sats,
                    }
                })
                .collect();

            let output = ListPendingTxsOutput {
                pending_txs: entries,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Commands::SubmitOnly { address, amount } => {
            let amount = Amount::from_sat(*amount);

            let (vtxo_list, script_pubkey_to_vtxo_map) =
                client.list_vtxos().await.map_err(|e| anyhow!(e))?;

            let spendable = vtxo_list
                .spendable_offchain()
                .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                    outpoint: vtxo.outpoint,
                    script_pubkey: vtxo.script.clone(),
                    expire_at: vtxo.expires_at,
                    amount: vtxo.amount,
                    assets: Vec::new(),
                })
                .collect::<Vec<_>>();

            let selected = ark_core::coin_select::select_vtxos(
                spendable,
                amount,
                client.server_info.dust,
                true,
            )
            .map_err(|e| anyhow!(e))?;

            let vtxo_inputs: Vec<VtxoInput> = selected
                .into_iter()
                .map(|coin| {
                    let vtxo = script_pubkey_to_vtxo_map
                        .get(&coin.script_pubkey)
                        .ok_or_else(|| {
                            anyhow!("missing VTXO for script pubkey: {}", coin.script_pubkey)
                        })?;
                    let (forfeit_script, control_block) = vtxo
                        .forfeit_spend_info()
                        .context("failed to get forfeit spend info")?;
                    Ok(VtxoInput::new(
                        forfeit_script,
                        None,
                        control_block,
                        vtxo.tapscripts(),
                        vtxo.script_pubkey(),
                        coin.amount,
                        coin.outpoint,
                        coin.assets,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;

            let ark_txid = client
                .submit_offchain_tx(vtxo_inputs, address.0, amount)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(%ark_txid, "Submitted offchain tx WITHOUT finalizing");
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ark_txid": ark_txid.to_string(),
                    "status": "submitted_not_finalized"
                }))?
            );
        }
        Commands::ContinuePendingTxs => {
            let finalized = client
                .continue_pending_offchain_txs()
                .await
                .map_err(|e| anyhow!(e))?;

            if finalized.is_empty() {
                let output = serde_json::json!({
                    "finalized_txids": [],
                    "message": "No pending transactions to finalize"
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                let output = serde_json::json!({
                    "finalized_txids": finalized.iter().map(|t| t.to_string()).collect::<Vec<_>>()
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        Commands::ListPendingSwapTxs => {
            let pending = client
                .list_pending_vhtlc_spend_txs()
                .await
                .map_err(|e| anyhow!(e))?;

            let entries: Vec<_> = pending
                .iter()
                .map(|tx| {
                    let swap_id = tx.spend_type.swap_id().to_string();
                    let ark_txid = tx.pending_tx.ark_txid.to_string();
                    let spend_type = match &tx.spend_type {
                        ark_client::PendingVhtlcSpendType::Claim { .. } => "claim",
                        ark_client::PendingVhtlcSpendType::CollaborativeRefund { .. } => {
                            "collaborative_refund"
                        }
                        ark_client::PendingVhtlcSpendType::ExpiredRefund { .. } => "expired_refund",
                    };
                    serde_json::json!({
                        "swap_id": swap_id,
                        "ark_txid": ark_txid,
                        "spend_type": spend_type,
                    })
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        Commands::ContinuePendingSwapTxs { swap_id } => {
            let pending = client
                .list_pending_vhtlc_spend_txs()
                .await
                .map_err(|e| anyhow!(e))?;

            let matched = pending
                .into_iter()
                .find(|tx| tx.spend_type.swap_id() == swap_id.as_str());

            match matched {
                None => {
                    anyhow::bail!(
                        "No pending VHTLC swap transaction found with swap_id: {swap_id}"
                    );
                }
                Some(pending_tx) => {
                    let txid = client
                        .continue_pending_vhtlc_spend_tx(&pending_tx)
                        .await
                        .map_err(|e| anyhow!(e))?;

                    let output = serde_json::json!({
                        "swap_id": swap_id,
                        "finalized_txid": txid.to_string(),
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
            }
        }
        Commands::CreateNote { .. } => {
            // Handled in main() before client setup
            unreachable!("CreateNote is handled before client initialization");
        }
        Commands::SendAssets {
            address,
            asset_id,
            amount,
        } => {
            let asset_id = asset_id.parse()?;

            let receiver = SendReceiver {
                address: address.0,
                amount: client.dust(),
                assets: vec![ark_core::server::Asset {
                    asset_id,
                    amount: *amount,
                }],
            };

            let txid = client.send(vec![receiver]).await.map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "ark_txid": txid.to_string(),
                })
            );
        }
        Commands::BurnAsset { asset_id, amount } => {
            let asset_id = asset_id.parse()?;

            let txid = client
                .burn_asset(asset_id, *amount)
                .await
                .map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "ark_txid": txid.to_string(),
                })
            );
        }
        Commands::ReissueAsset { asset_id, amount } => {
            let asset_id = asset_id.parse()?;

            let txid = client
                .reissue_asset(asset_id, *amount)
                .await
                .map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "ark_txid": txid.to_string(),
                })
            );
        }
        Commands::GetAsset { asset_id } => {
            let asset_id = asset_id.parse()?;

            let asset_info = client.get_asset(asset_id).await.map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "asset_id": asset_info.asset_id,
                    "control_asset_id": asset_info.control_asset_id,
                    "supply": asset_info.supply,
                    "metadata": asset_info.metadata,
                })
            );
        }
        Commands::IssueAsset {
            amount,
            control_amount,
            control_asset_id,
            metadata,
        } => {
            let control_asset = match (control_amount, control_asset_id) {
                (Some(_), Some(_)) => {
                    bail!("specify either --control-amount or --control-asset-id, not both")
                }
                (Some(amt), None) => {
                    let config = ControlAssetConfig::new(*amt)
                        .context("control asset amount must be non-zero")?;

                    Some(config)
                }
                (None, Some(id)) => {
                    let control_asset_id = id.parse().context("invalid control asset ID")?;

                    Some(ControlAssetConfig::existing(control_asset_id))
                }
                (None, None) => None,
            };

            let metadata = metadata.clone().map(|m| {
                m.split(',')
                    .filter_map(|pair| {
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next()?.trim().to_string();
                        let value = parts.next()?.trim().to_string();
                        Some((key, value))
                    })
                    .collect::<Vec<_>>()
            });

            let result = client
                .issue_asset(*amount, control_asset, metadata)
                .await
                .map_err(|e| anyhow!(e))?;

            println!(
                "{}",
                serde_json::json!({
                    "ark_txid": result.ark_txid.to_string(),
                    "asset_ids": result.asset_ids,
                })
            );
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
