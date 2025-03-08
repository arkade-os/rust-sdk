#![allow(clippy::print_stdout)]
#![allow(clippy::large_enum_variant)]

use anyhow::bail;
use anyhow::Result;
use ark_core::boarding_output::list_boarding_outpoints;
use ark_core::boarding_output::BoardingOutpoints;
use ark_core::coin_select::select_vtxos;
use ark_core::generate_incoming_vtxo_transaction_history;
use ark_core::generate_outgoing_vtxo_transaction_history;
use ark_core::redeem;
use ark_core::redeem::build_redeem_transaction;
use ark_core::redeem::sign_redeem_transaction;
use ark_core::round;
use ark_core::round::create_and_sign_forfeit_txs;
use ark_core::round::generate_nonce_tree;
use ark_core::round::sign_round_psbt;
use ark_core::round::sign_vtxo_tree;
use ark_core::server::RoundInput;
use ark_core::server::RoundOutput;
use ark_core::server::RoundStreamEvent;
use ark_core::server::VtxoOutPoint;
use ark_core::vtxo::list_virtual_tx_outpoints;
use ark_core::vtxo::VirtualTxOutpoints;
use ark_core::ArkAddress;
use ark_core::ArkTransaction;
use ark_core::BoardingOutput;
use ark_core::ExplorerUtxo;
use ark_core::Vtxo;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use clap::Parser;
use clap::Subcommand;
use futures::StreamExt;
use jiff::Timestamp;
use rand::thread_rng;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::str::FromStr;
use tokio::task::block_in_place;

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
    /// Send coins to an Ark address.
    SendToArkAddress {
        /// Where to send the coins too.
        address: ArkAddressCli,
        /// How many sats to send.
        amount: u64,
    },
    /// Transform boarding outputs and VTXOs into fresh, confirmed VTXOs.
    Settle,
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

#[derive(Deserialize)]
struct Config {
    ark_server_url: String,
    esplora_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    let seed = fs::read_to_string(cli.seed)?;
    let sk = SecretKey::from_str(&seed)?;

    let config = fs::read_to_string(cli.config)?;
    let config: Config = toml::from_str(&config)?;

    let secp = Secp256k1::new();

    let pk = PublicKey::from_secret_key(&secp, &sk);

    let ark_server_url = config.ark_server_url;
    let mut grpc_client = ark_grpc::Client::new(ark_server_url);

    grpc_client.connect().await?;

    let server_info = grpc_client.get_info().await?;

    let esplora_client = EsploraClient::new(&config.esplora_url)?;

    // In this example we use the same script for all VTXOs.
    let vtxo = Vtxo::new(
        &secp,
        server_info.pk.x_only_public_key().0,
        pk.x_only_public_key().0,
        vec![],
        server_info.unilateral_exit_delay,
        server_info.network,
    )?;

    // In this example we use the same script for all boarding outputs.
    let boarding_output = BoardingOutput::new(
        &secp,
        server_info.pk.x_only_public_key().0,
        pk.x_only_public_key().0,
        server_info.unilateral_exit_delay,
        server_info.network,
    );

    let runtime = tokio::runtime::Handle::current();
    let find_outpoints_fn =
        |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
            block_in_place(|| {
                runtime.block_on(async {
                    let outpoints = esplora_client
                        .find_outpoints(address)
                        .await
                        .map_err(ark_core::Error::ad_hoc)?;

                    Ok(outpoints)
                })
            })
        };

    match &cli.command {
        Commands::Balance => {
            let virtual_tx_outpoints = {
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo]).await?;
                list_virtual_tx_outpoints(find_outpoints_fn, spendable_vtxos)?
            };
            let boarding_outpoints =
                list_boarding_outpoints(find_outpoints_fn, &[boarding_output])?;

            println!(
                "Offchain balance: spendable = {}, expired = {}",
                virtual_tx_outpoints.spendable_balance(),
                virtual_tx_outpoints.expired_balance()
            );
            println!(
                "Boarding balance: spendable = {}, expired = {}, pending = {}",
                boarding_outpoints.spendable_balance(),
                boarding_outpoints.expired_balance(),
                boarding_outpoints.pending_balance()
            );
        }
        Commands::TransactionHistory => {
            let txs: Vec<ArkTransaction> = transaction_history(
                &grpc_client,
                &esplora_client,
                &[boarding_output.address().clone()],
                &[vtxo],
            )
            .await?;

            for tx in txs.iter() {
                println!("{}\n", pretty_print_transaction(tx)?);
            }
        }
        Commands::BoardingAddress => {
            let boarding_address = boarding_output.address();

            println!("Send coins to this on-chain address: {boarding_address}\n");
            println!(
                "Once confirmed, you will have {} seconds to exchange the boarding output for a VTXO.",
                boarding_output.exit_delay_duration().as_secs()
            );
        }
        Commands::OffchainAddress => {
            let offchain_address = vtxo.to_ark_address();

            println!("Send VTXOs to this offchain address: {offchain_address}\n");
        }
        Commands::SendToArkAddress { address, amount } => {
            let amount = Amount::from_sat(*amount);

            let virtual_tx_outpoints = {
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo.clone()]).await?;
                list_virtual_tx_outpoints(find_outpoints_fn, spendable_vtxos)?
            };

            let vtxo_outpoints = virtual_tx_outpoints
                .spendable
                .iter()
                .map(|(outpoint, _)| ark_core::coin_select::VtxoOutPoint {
                    outpoint: outpoint.outpoint,
                    expire_at: outpoint.expire_at,
                    amount: outpoint.amount,
                })
                .collect::<Vec<_>>();

            let selected_outpoints = select_vtxos(vtxo_outpoints, amount, server_info.dust, true)?;

            let vtxo_inputs = virtual_tx_outpoints
                .spendable
                .into_iter()
                .filter(|(outpoint, _)| {
                    selected_outpoints
                        .iter()
                        .any(|o| o.outpoint == outpoint.outpoint)
                })
                .map(|(outpoint, vtxo)| {
                    redeem::VtxoInput::new(vtxo, outpoint.amount, outpoint.outpoint)
                })
                .collect::<Vec<_>>();

            let change_address = vtxo.to_ark_address();

            let secp = Secp256k1::new();
            let kp = Keypair::from_secret_key(&secp, &sk);

            let mut redeem_psbt = build_redeem_transaction(
                &[(&address.0, amount)],
                Some(&change_address),
                &vtxo_inputs,
            )?;

            let sign_fn =
                |msg: secp256k1::Message| -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                    let pk = kp.x_only_public_key().0;

                    Ok((sig, pk))
                };

            for (i, _) in vtxo_inputs.iter().enumerate() {
                sign_redeem_transaction(sign_fn, &mut redeem_psbt, &vtxo_inputs, i)?;
            }

            let psbt = grpc_client
                .submit_redeem_transaction(redeem_psbt.clone())
                .await?;

            let txid = psbt.extract_tx()?.compute_txid();

            println!("Sent {amount} to {} in transaction {txid}", address.0);
        }
        Commands::Settle => {
            let virtual_tx_outpoints = {
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo.clone()]).await?;
                list_virtual_tx_outpoints(find_outpoints_fn, spendable_vtxos)?
            };
            let boarding_outpoints =
                list_boarding_outpoints(find_outpoints_fn, &[boarding_output])?;

            let res = settle(
                &grpc_client,
                &server_info,
                sk,
                virtual_tx_outpoints,
                boarding_outpoints,
                vtxo.to_ark_address(),
            )
            .await;

            match res {
                Ok(Some(txid)) => {
                    println!(
                        "Settled boarding outputs and VTXOs into new VTXOs.\n\n Round TXID: {txid}\n"
                    );
                }
                Ok(None) => {
                    println!("No boarding outputs or VTXOs can be settled at the moment.");
                }
                Err(e) => {
                    println!("Failed to settle boarding outputs and VTXOs: {e:#}");
                }
            }
        }
    }

    Ok(())
}

async fn spendable_vtxos(
    grpc_client: &ark_grpc::Client,
    vtxos: &[Vtxo],
) -> Result<HashMap<Vtxo, Vec<VtxoOutPoint>>> {
    let mut spendable_vtxos = HashMap::new();
    for vtxo in vtxos.iter() {
        // The VTXOs for the given Ark address that the Ark server tells us about.
        let vtxo_outpoints = grpc_client.list_vtxos(&vtxo.to_ark_address()).await?;

        spendable_vtxos.insert(vtxo.clone(), vtxo_outpoints.spendable);
    }

    Ok(spendable_vtxos)
}

async fn settle(
    grpc_client: &ark_grpc::Client,
    server_info: &ark_core::server::Info,
    sk: SecretKey,
    vtxos: VirtualTxOutpoints,
    boarding_outputs: BoardingOutpoints,
    to_address: ArkAddress,
) -> Result<Option<Txid>> {
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    if vtxos.spendable.is_empty() && boarding_outputs.spendable.is_empty() {
        return Ok(None);
    }

    let cosigner_kp = Keypair::new(&secp, &mut rng);

    let round_inputs = {
        let boarding_inputs = boarding_outputs
            .spendable
            .clone()
            .into_iter()
            .map(|o| RoundInput::new(o.0, o.2.tapscripts()));

        let vtxo_inputs = vtxos
            .spendable
            .clone()
            .into_iter()
            .map(|v| RoundInput::new(v.0.outpoint, v.1.tapscripts()));

        boarding_inputs.chain(vtxo_inputs).collect::<Vec<_>>()
    };

    let payment_id = grpc_client
        .register_inputs_for_next_round(&round_inputs)
        .await?;

    tracing::info!(
        payment_id,
        n_round_inputs = round_inputs.len(),
        "Registered round inputs"
    );

    let spendable_amount = boarding_outputs.spendable_balance() + vtxos.spendable_balance();

    let round_outputs = vec![RoundOutput::new_virtual(to_address, spendable_amount)];
    grpc_client
        .register_outputs_for_next_round(
            payment_id.clone(),
            &round_outputs,
            &[cosigner_kp.public_key()],
            false,
        )
        .await?;

    tracing::info!(
        n_round_outputs = round_outputs.len(),
        "Registered round outputs"
    );

    grpc_client.ping(payment_id).await?;

    let mut event_stream = grpc_client.get_event_stream().await?;

    let round_signing_event = match event_stream.next().await {
        Some(Ok(RoundStreamEvent::RoundSigning(e))) => e,
        other => bail!("Did not get round signing event: {other:?}"),
    };

    let round_id = round_signing_event.id;

    tracing::info!(round_id, "Round signing started");

    let unsigned_vtxo_tree = round_signing_event
        .unsigned_vtxo_tree
        .expect("to have an unsigned VTXO tree");

    let nonce_tree = generate_nonce_tree(&mut rng, &unsigned_vtxo_tree, cosigner_kp.public_key())?;

    grpc_client
        .submit_tree_nonces(
            &round_id,
            cosigner_kp.public_key(),
            nonce_tree.to_pub_nonce_tree().into_inner(),
        )
        .await?;

    let round_signing_nonces_generated_event = match event_stream.next().await {
        Some(Ok(RoundStreamEvent::RoundSigningNoncesGenerated(e))) => e,
        other => bail!("Did not get round signing nonces generated event: {other:?}"),
    };

    let round_id = round_signing_nonces_generated_event.id;

    let agg_pub_nonce_tree = round_signing_nonces_generated_event.tree_nonces;

    tracing::info!(round_id, "Round combined nonces generated");

    let partial_sig_tree = sign_vtxo_tree(
        server_info.vtxo_tree_expiry,
        server_info.pk.x_only_public_key().0,
        &cosigner_kp,
        &unsigned_vtxo_tree,
        &round_signing_event.unsigned_round_tx,
        nonce_tree,
        &agg_pub_nonce_tree.into(),
    )?;

    grpc_client
        .submit_tree_signatures(
            &round_id,
            cosigner_kp.public_key(),
            partial_sig_tree.into_inner(),
        )
        .await?;

    let round_finalization_event = match event_stream.next().await {
        Some(Ok(RoundStreamEvent::RoundFinalization(e))) => e,
        other => bail!("Did not get round finalization event: {other:?}"),
    };

    let round_id = round_finalization_event.id;

    tracing::info!(round_id, "Round finalization started");

    let vtxo_inputs = vtxos
        .spendable
        .into_iter()
        .map(|(outpoint, vtxo)| round::VtxoInput::new(vtxo, outpoint.amount, outpoint.outpoint))
        .collect::<Vec<_>>();

    let keypair = Keypair::from_secret_key(&secp, &sk);
    let signed_forfeit_psbts = create_and_sign_forfeit_txs(
        &keypair,
        vtxo_inputs.as_slice(),
        round_finalization_event.connector_tree,
        &round_finalization_event.connectors_index,
        round_finalization_event.min_relay_fee_rate,
        &server_info.forfeit_address,
        server_info.dust,
    )?;

    let onchain_inputs = boarding_outputs
        .spendable
        .into_iter()
        .map(|(outpoint, _, boarding_output)| round::OnChainInput::new(boarding_output, outpoint))
        .collect::<Vec<_>>();

    let round_psbt = if round_inputs.is_empty() {
        None
    } else {
        let mut round_psbt = round_finalization_event.round_tx;

        let sign_for_pk_fn = |_: &XOnlyPublicKey,
                              msg: &secp256k1::Message|
         -> Result<schnorr::Signature, ark_core::Error> {
            Ok(secp.sign_schnorr_no_aux_rand(msg, &keypair))
        };

        sign_round_psbt(sign_for_pk_fn, &mut round_psbt, &onchain_inputs)?;

        Some(round_psbt)
    };

    grpc_client
        .submit_signed_forfeit_txs(signed_forfeit_psbts, round_psbt)
        .await?;

    let round_finalized_event = match event_stream.next().await {
        Some(Ok(RoundStreamEvent::RoundFinalized(e))) => e,
        other => bail!("Did not get round finalized event: {other:?}"),
    };

    let round_id = round_finalized_event.id;

    tracing::info!(round_id, "Round finalized");

    Ok(Some(round_finalized_event.round_txid))
}

pub struct EsploraClient {
    esplora_client: esplora_client::AsyncClient,
}

#[derive(Clone, Copy, Debug)]
pub struct SpendStatus {
    pub spend_txid: Option<Txid>,
}

impl EsploraClient {
    pub fn new(url: &str) -> Result<Self> {
        let builder = esplora_client::Builder::new(url);
        let esplora_client = builder.build_async()?;

        Ok(Self { esplora_client })
    }

    async fn find_outpoints(&self, address: &bitcoin::Address) -> Result<Vec<ExplorerUtxo>> {
        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .await?;

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
                .await?;

            match status {
                Some(esplora_client::OutputStatus { spent: false, .. }) | None => {
                    utxos.push(*output);
                }
                Some(esplora_client::OutputStatus { spent: true, .. }) => {
                    utxos.push(ExplorerUtxo {
                        is_spent: true,
                        ..*output
                    });
                }
            }
        }

        Ok(utxos)
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .await?;

        Ok(SpendStatus {
            spend_txid: status.and_then(|s| s.txid),
        })
    }
}

async fn transaction_history(
    grpc_client: &ark_grpc::Client,
    onchain_explorer: &EsploraClient,
    boarding_addresses: &[bitcoin::Address],
    vtxos: &[Vtxo],
) -> Result<Vec<ArkTransaction>> {
    let mut boarding_transactions = Vec::new();
    let mut boarding_round_transactions = Vec::new();

    for boarding_address in boarding_addresses.iter() {
        let outpoints = onchain_explorer.find_outpoints(boarding_address).await?;

        for ExplorerUtxo {
            outpoint,
            amount,
            confirmation_blocktime,
            ..
        } in outpoints.iter()
        {
            let confirmed_at = confirmation_blocktime.map(|t| t as i64);

            boarding_transactions.push(ArkTransaction::Boarding {
                txid: outpoint.txid,
                amount: *amount,
                confirmed_at,
            });

            let status = onchain_explorer
                .get_output_status(&outpoint.txid, outpoint.vout)
                .await?;

            if let Some(spend_txid) = status.spend_txid {
                boarding_round_transactions.push(spend_txid);
            }
        }
    }

    let mut incoming_transactions = Vec::new();
    let mut outgoing_transactions = Vec::new();
    for vtxo in vtxos.iter() {
        let vtxos = grpc_client.list_vtxos(&vtxo.to_ark_address()).await?;

        let mut new_incoming_transactions = generate_incoming_vtxo_transaction_history(
            &vtxos.spent,
            &vtxos.spendable,
            &boarding_round_transactions,
        )?;

        incoming_transactions.append(&mut new_incoming_transactions);

        let mut new_outgoing_transactions =
            generate_outgoing_vtxo_transaction_history(&vtxos.spent, &vtxos.spendable)?;

        outgoing_transactions.append(&mut new_outgoing_transactions);
    }

    let mut txs = [
        boarding_transactions,
        incoming_transactions,
        outgoing_transactions,
    ]
    .concat();

    txs.sort_by_key(|b| std::cmp::Reverse(b.created_at()));

    Ok(txs)
}

fn pretty_print_transaction(tx: &ArkTransaction) -> Result<String> {
    let print_str = match tx {
        ArkTransaction::Boarding {
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
        ArkTransaction::Round {
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
                "Type: Round\n\
                 TXID: {txid}\n\
                 Status: {status}\n\
                 Amount: {amount}\n\
                 Time: {time}"
            )
        }
        ArkTransaction::Redeem {
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
                "Type: Redeem\n\
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
