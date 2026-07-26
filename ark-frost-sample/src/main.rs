#![allow(clippy::print_stdout)]
// The copy-paste coordination between actor terminals is driven by stderr prompts.
#![allow(clippy::print_stderr)]

//! An Ark Lightning client whose wallet key is a FROST 2-of-3 multisig.
//!
//! Three actors — alice, bob and clair — each hold a share of a single group key generated with
//! distributed key generation (no party ever knows the full secret key). The Arkade address is
//! derived from the x-only group public key, so on-chain and to the Ark server it is
//! indistinguishable from a single-key wallet.
//!
//! Every signature requires 2 of the 3 actors: the coordinator prints a signing request blob and
//! one other actor answers it with `sign` in their own terminal. See the README for a full
//! three-terminal walkthrough of receiving and sending over Lightning.

mod frost;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use ark_bdk_wallet::Wallet;
use ark_client::lightning_invoice::Bolt11Invoice;
use ark_client::Blockchain;
use ark_client::Error;
use ark_client::OfflineClient;
use ark_client::OfflineClientConfig;
use ark_client::SpendStatus;
use ark_client::SqliteSwapStorage;
use ark_client::SwapAmount;
use ark_client::TxStatus;
use ark_core::ExplorerUtxo;
use bitcoin::key::Secp256k1;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::Transaction;
use bitcoin::Txid;
use clap::Parser;
use clap::Subcommand;
use frost::FrostActor;
use frost::FrostSigner;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(name = "ark-frost-sample")]
#[command(about = "An Ark Lightning client backed by a FROST 2-of-3 multisig")]
struct Cli {
    /// Which actor this terminal represents.
    #[arg(short, long, value_parser = ["alice", "bob", "clair"])]
    actor: String,

    /// Data directory for this actor (key share, config, swap storage).
    /// Defaults to ./<actor>.
    #[arg(short, long)]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the interactive 2-of-3 distributed key generation.
    ///
    /// All three actors must run this at the same time in separate terminals and exchange the
    /// printed blobs.
    Keygen,
    /// Show the FROST group public key.
    GroupInfo,
    /// Show the Ark address of the FROST multisig wallet.
    OffchainAddress,
    /// Show the balance of the FROST multisig wallet.
    Balance,
    /// Generate a BOLT11 invoice to receive via Lightning (Boltz reverse submarine swap).
    ///
    /// Waits for the invoice to be paid and claims the funds; claiming requires a signing
    /// ceremony with one other actor.
    Invoice {
        /// How many sats to receive.
        amount: u64,
    },
    /// Pay a BOLT11 invoice via a Boltz submarine swap.
    ///
    /// Funding the swap requires signing ceremonies with one other actor.
    Pay {
        /// A BOLT11 invoice.
        invoice: String,
    },
    /// Participate in a signing ceremony started by another actor.
    Sign {
        /// The signing request blob printed by the coordinator.
        blob: String,
    },
    /// Run a signing ceremony over an arbitrary 32-byte message, without an Ark server.
    ///
    /// Useful to verify the keygen output and the copy-paste signing flow end to end.
    SignTest {
        /// The 32-byte message to sign, hex-encoded.
        msg: String,
    },
}

#[derive(Deserialize)]
struct Config {
    ark_server_url: String,
    esplora_url: String,
    boltz_url: String,
    swap_storage_path: Option<String>,
    network: Option<String>,
}

impl Config {
    fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("ark.config.toml");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        toml::from_str(&content).context("invalid config")
    }

    fn network(&self) -> Result<Network> {
        match self.network.as_deref() {
            None | Some("regtest") => Ok(Network::Regtest),
            Some(other) => Network::from_str(other).context("invalid network"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("failed to install crypto providers"))?;

    let cli = Cli::parse();
    let dir = cli
        .dir
        .unwrap_or_else(|| PathBuf::from(format!("./{}", cli.actor)));

    match cli.command {
        Commands::Keygen => {
            frost::run_dkg(&cli.actor, &dir)?;
        }
        Commands::GroupInfo => {
            let actor = FrostActor::load(&cli.actor, &dir)?;
            println!(
                "{}",
                serde_json::json!({ "group_pk": actor.group_pk().to_string() })
            );
        }
        Commands::Sign { blob } => {
            let actor = FrostActor::load(&cli.actor, &dir)?;
            actor.respond_to_signing_request(&blob)?;
        }
        Commands::SignTest { msg } => {
            let actor = FrostActor::load(&cli.actor, &dir)?;
            let msg: [u8; 32] =
                bitcoin::hex::FromHex::from_hex(&msg).context("message must be 32 bytes of hex")?;
            let signature = actor.coordinate_signature(&msg)?;
            println!(
                "{}",
                serde_json::json!({ "signature": signature.to_string() })
            );
        }
        Commands::OffchainAddress => {
            let client = connect(&cli.actor, &dir).await?;
            let (address, _) = client
                .get_offchain_address()
                .await
                .map_err(|e| anyhow!(e))?;
            println!("{}", serde_json::json!({ "address": address.encode() }));
        }
        Commands::Balance => {
            let client = connect(&cli.actor, &dir).await?;
            let balance = client.offchain_balance().await.map_err(|e| anyhow!(e))?;
            println!(
                "{}",
                serde_json::json!({
                    "offchain_confirmed": balance.confirmed(),
                    "offchain_pre_confirmed": balance.pre_confirmed(),
                    "recoverable": balance.recoverable(),
                })
            );
        }
        Commands::Invoice { amount } => {
            let client = connect(&cli.actor, &dir).await?;

            let res = client
                .get_ln_invoice(SwapAmount::invoice(Amount::from_sat(amount)), None, None)
                .await
                .map_err(|e| anyhow!(e))?;

            let invoice = res.invoice.to_string();
            let swap_id = res.swap_id;

            tracing::info!(swap_id, "Generated Lightning invoice");
            println!(
                "{}",
                serde_json::json!({ "invoice": invoice, "swap_id": swap_id })
            );

            eprintln!("\nWaiting for the invoice to be paid...");
            eprintln!("Claiming the funds will require a signing ceremony with one other actor.");

            client
                .wait_for_vhtlc(&swap_id)
                .await
                .map_err(|e| anyhow!(e))?;

            tracing::info!(
                swap_id,
                "Lightning invoice paid and claimed by the multisig"
            );
        }
        Commands::Pay { invoice } => {
            let client = connect(&cli.actor, &dir).await?;

            let invoice = Bolt11Invoice::from_str(&invoice)
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
    }

    Ok(())
}

async fn connect(
    actor: &str,
    dir: &Path,
) -> Result<ark_client::Client<EsploraClient, Wallet, SqliteSwapStorage>> {
    let config = Config::load(dir)?;
    let network = config.network()?;

    let frost_actor = FrostActor::load(actor, dir)?;
    let signer = Arc::new(FrostSigner::new(frost_actor));
    tracing::info!(group_pk = %signer.group_pk(), "Loaded FROST key share");

    let esplora_client = Arc::new(EsploraClient::new(&config.esplora_url)?);

    // The on-chain wallet is required by the client but unused in the Lightning-only flows; give
    // it a throwaway key.
    let secp = Secp256k1::new();
    let onchain_kp = bitcoin::key::Keypair::new(&secp, &mut rand::thread_rng());
    let wallet = Arc::new(Wallet::new(
        onchain_kp,
        network,
        config.esplora_url.as_str(),
    )?);

    let swap_storage_path = config.swap_storage_path.clone().unwrap_or_else(|| {
        dir.join("swap_storage.sqlite")
            .to_string_lossy()
            .into_owned()
    });
    let storage = Arc::new(
        SqliteSwapStorage::new(&swap_storage_path)
            .await
            .map_err(|e| anyhow!(e))?,
    );

    let client_config = OfflineClientConfig {
        ark_server_url: config.ark_server_url.clone(),
        boltz_url: config.boltz_url.clone(),
        ..Default::default()
    };

    let offline_client =
        OfflineClient::with_signer(client_config, signer, esplora_client, wallet, storage);

    offline_client.connect().await.map_err(|e| anyhow!(e))
}

pub struct EsploraClient {
    esplora_client: esplora_client::AsyncClient,
}

impl EsploraClient {
    pub fn new(url: &str) -> Result<Self> {
        let builder = esplora_client::Builder::new(url);
        let esplora_client = builder.build_async()?;

        Ok(Self { esplora_client })
    }
}

impl Blockchain for EsploraClient {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        let current_block_height = self
            .esplora_client
            .get_height()
            .await
            .map_err(Error::consumer)?;

        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .await
            .map_err(Error::consumer)?;

        let spent_outpoints: HashSet<OutPoint> = txs
            .iter()
            .flat_map(|tx| {
                tx.vin
                    .iter()
                    .filter(|input| {
                        input
                            .prevout
                            .as_ref()
                            .is_some_and(|prevout| prevout.scriptpubkey == script_pubkey)
                    })
                    .map(|input| OutPoint {
                        txid: input.txid,
                        vout: input.vout,
                    })
            })
            .collect();

        let utxos = txs
            .into_iter()
            .flat_map(|tx| {
                let txid = tx.txid;
                tx.vout
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.scriptpubkey == script_pubkey)
                    .map(|(i, v)| {
                        let outpoint = OutPoint {
                            txid,
                            vout: i as u32,
                        };
                        let confirmations = match tx.status.block_height {
                            Some(confirmation_block_height) => {
                                match current_block_height.checked_sub(confirmation_block_height) {
                                    Some(x) => x + 1,
                                    None => 0,
                                }
                            }
                            None => 0,
                        };

                        ExplorerUtxo {
                            outpoint,
                            amount: Amount::from_sat(v.value),
                            confirmation_blocktime: tx.status.block_time,
                            confirmations: confirmations as u64,
                            is_spent: spent_outpoints.contains(&outpoint),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

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

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,\
                 tower=info,\
                 hyper_util=info,\
                 hyper=info,\
                 h2=warn,\
                 reqwest=info,\
                 ark_core=info,\
                 rustls=info,\
                 sqlx::query=warn"
                    .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init()
}
