#![allow(clippy::print_stdout)]
#![allow(clippy::large_enum_variant)]

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use ark_core::batch;
use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::batch::generate_nonce_tree;
use ark_core::batch::sign_batch_tree;
use ark_core::batch::sign_commitment_psbt;
use ark_core::boarding_output::list_boarding_outpoints;
use ark_core::boarding_output::BoardingOutpoints;
use ark_core::coin_select::select_vtxos;
use ark_core::history;
use ark_core::history::generate_incoming_vtxo_transaction_history;
use ark_core::history::generate_outgoing_vtxo_transaction_history;
use ark_core::history::sort_transactions_by_created_at;
use ark_core::proof_of_funds;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::server::BatchTreeEventType;
use ark_core::server::GetVtxosRequest;
use ark_core::server::StreamEvent;
use ark_core::server::VirtualTxOutPoint;
use ark_core::vtxo::list_virtual_tx_outpoints;
use ark_core::vtxo::VirtualTxOutPoints;
use ark_core::ArkAddress;
use ark_core::BoardingOutput;
use ark_core::ExplorerUtxo;
use ark_core::TxGraph;
use ark_core::Vtxo;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::SecretKey;
use bitcoin::Amount;
use bitcoin::Denomination;
use bitcoin::OutPoint;
use bitcoin::TxOut;
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
    /// Send coins to an Ark address.
    BatchSend {
        /// Where to send the coins too.
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
            bail!("Invalid input provided");
        }

        let addresses_and_amounts: Vec<(ArkAddress, Amount)> = parts
            .chunks(2)
            .map(|chunk| {
                (
                    ArkAddress::decode(chunk[0]).expect("Failed to decode address"),
                    Amount::from_str_in(chunk[1], Denomination::Satoshi)
                        .expect("Failed to parse Amount"),
                )
            })
            .collect();

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
        server_info.boarding_exit_delay,
        server_info.network,
    )?;

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
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo], false).await?;
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
            let txs: Vec<history::Transaction> = transaction_history(
                &grpc_client,
                &esplora_client,
                &[boarding_output.address().clone()],
                &[vtxo],
            )
            .await?;

            if txs.is_empty() {
                println!("No transactions found");
            }

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
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo.clone()], false).await?;
                list_virtual_tx_outpoints(find_outpoints_fn, spendable_vtxos)?
            };

            let selected_outpoints = {
                let virtual_tx_outpoints = virtual_tx_outpoints
                    .spendable
                    .iter()
                    .map(|(outpoint, _)| ark_core::coin_select::VirtualTxOutPoint {
                        outpoint: outpoint.outpoint,
                        expire_at: outpoint.expires_at,
                        amount: outpoint.amount,
                    })
                    .collect::<Vec<_>>();

                select_vtxos(virtual_tx_outpoints, amount, server_info.dust, true)?
            };

            let vtxo_inputs = virtual_tx_outpoints
                .spendable
                .into_iter()
                .filter(|(outpoint, _)| {
                    selected_outpoints
                        .iter()
                        .any(|o| o.outpoint == outpoint.outpoint)
                })
                .map(|(outpoint, vtxo)| {
                    send::VtxoInput::new(vtxo, outpoint.amount, outpoint.outpoint)
                })
                .collect::<Vec<_>>();

            let change_address = vtxo.to_ark_address();

            let secp = Secp256k1::new();
            let kp = Keypair::from_secret_key(&secp, &sk);

            let OffchainTransactions {
                mut ark_tx,
                checkpoint_txs,
            } = build_offchain_transactions(
                &[(&address.0, amount)],
                Some(&change_address),
                &vtxo_inputs,
                server_info.dust,
            )?;

            let sign_fn =
                |msg: secp256k1::Message| -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                    let pk = kp.x_only_public_key().0;

                    Ok((sig, pk))
                };

            for i in 0..checkpoint_txs.len() {
                sign_ark_transaction(
                    sign_fn,
                    &mut ark_tx,
                    &checkpoint_txs
                        .iter()
                        .map(|(_, output, outpoint, _)| (output.clone(), *outpoint))
                        .collect::<Vec<_>>(),
                    i,
                )?;
            }

            let ark_txid = ark_tx.unsigned_tx.compute_txid();

            let mut res = grpc_client
                .submit_offchain_transaction_request(
                    ark_tx,
                    checkpoint_txs
                        .into_iter()
                        .map(|(psbt, _, _, _)| psbt)
                        .collect(),
                )
                .await
                .context("failed to submit offchain transaction request")?;

            for checkpoint_psbt in res.signed_checkpoint_txs.iter_mut() {
                let vtxo_input = vtxo_inputs
                    .iter()
                    .find(|input| {
                        checkpoint_psbt.unsigned_tx.input[0].previous_output == input.outpoint()
                    })
                    .with_context(|| {
                        format!(
                            "could not find VTXO input for checkpoint transaction {}",
                            checkpoint_psbt.unsigned_tx.compute_txid(),
                        )
                    })?;

                sign_checkpoint_transaction(sign_fn, checkpoint_psbt, vtxo_input)?;
            }

            grpc_client
                .finalize_offchain_transaction(ark_txid, res.signed_checkpoint_txs)
                .await
                .context("failed to finalize offchain transaction")?;

            println!("Sent {amount} to {} in transaction {ark_txid}", address.0);
        }
        Commands::Settle => {
            let virtual_tx_outpoints = {
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo.clone()], true).await?;
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
                        "Settled boarding outputs and VTXOs into new VTXOs.\n\n Batch TXID: {txid}\n"
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
        Commands::BatchSend {
            addresses_and_amounts,
        } => {
            let addresses_and_amounts = addresses_and_amounts.0.clone();

            let total_amount = addresses_and_amounts
                .iter()
                .map(|(_, amount)| *amount)
                .sum();

            let virtual_tx_outpoints = {
                let spendable_vtxos = spendable_vtxos(&grpc_client, &[vtxo.clone()], false).await?;
                list_virtual_tx_outpoints(find_outpoints_fn, spendable_vtxos)?
            };

            let selected_outpoints = {
                let virtual_tx_outpoints = virtual_tx_outpoints
                    .spendable
                    .iter()
                    .map(|(outpoint, _)| ark_core::coin_select::VirtualTxOutPoint {
                        outpoint: outpoint.outpoint,
                        expire_at: outpoint.expires_at,
                        amount: outpoint.amount,
                    })
                    .collect::<Vec<_>>();

                select_vtxos(virtual_tx_outpoints, total_amount, server_info.dust, true)?
            };

            let vtxo_inputs = virtual_tx_outpoints
                .spendable
                .into_iter()
                .filter(|(outpoint, _)| {
                    selected_outpoints
                        .iter()
                        .any(|o| o.outpoint == outpoint.outpoint)
                })
                .map(|(outpoint, vtxo)| {
                    send::VtxoInput::new(vtxo, outpoint.amount, outpoint.outpoint)
                })
                .collect::<Vec<_>>();

            let change_address = vtxo.to_ark_address();

            let secp = Secp256k1::new();
            let kp = Keypair::from_secret_key(&secp, &sk);

            let outputs = addresses_and_amounts
                .iter()
                .map(|(address, amount)| (address, *amount))
                .collect::<Vec<_>>();

            let OffchainTransactions {
                mut ark_tx,
                checkpoint_txs,
            } = build_offchain_transactions(
                outputs.as_slice(),
                Some(&change_address),
                &vtxo_inputs,
                server_info.dust,
            )?;

            let sign_fn =
                |msg: secp256k1::Message| -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                    let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                    let pk = kp.x_only_public_key().0;

                    Ok((sig, pk))
                };

            for i in 0..checkpoint_txs.len() {
                sign_ark_transaction(
                    sign_fn,
                    &mut ark_tx,
                    &checkpoint_txs
                        .iter()
                        .map(|(_, output, outpoint, _)| (output.clone(), *outpoint))
                        .collect::<Vec<_>>(),
                    i,
                )?;
            }

            let ark_txid = ark_tx.unsigned_tx.compute_txid();

            let mut res = grpc_client
                .submit_offchain_transaction_request(
                    ark_tx,
                    checkpoint_txs
                        .into_iter()
                        .map(|(psbt, _, _, _)| psbt)
                        .collect(),
                )
                .await
                .context("failed to submit offchain transaction request")?;

            for checkpoint_psbt in res.signed_checkpoint_txs.iter_mut() {
                let vtxo_input = vtxo_inputs
                    .iter()
                    .find(|input| {
                        checkpoint_psbt.unsigned_tx.input[0].previous_output == input.outpoint()
                    })
                    .with_context(|| {
                        format!(
                            "could not find VTXO input for checkpoint transaction {}",
                            checkpoint_psbt.unsigned_tx.compute_txid(),
                        )
                    })?;

                sign_checkpoint_transaction(sign_fn, checkpoint_psbt, vtxo_input)?;
            }

            grpc_client
                .finalize_offchain_transaction(ark_txid, res.signed_checkpoint_txs)
                .await
                .context("failed to finalize offchain transaction")?;

            let all_addresses = addresses_and_amounts
                .iter()
                .map(|(address, _)| address.encode())
                .collect::<Vec<_>>();
            println!("Sent {total_amount} to {all_addresses:?} in transaction {ark_txid}",);
        }
        Commands::Subscribe { address } => {
            println!("Subscribing to address: {}", address.0);

            // First subscribe to the address to get a subscription ID
            let subscription_id = grpc_client
                .subscribe_to_scripts(vec![address.0.clone()], "".to_string())
                .await?;

            println!("Subscription ID: {}", subscription_id);

            // Now get the subscription stream
            let mut subscription_stream = grpc_client.get_subscription(subscription_id).await?;

            // FIXME: I never get called!!

            println!("Listening for notifications... Press Ctrl+C to stop");

            // Process subscription responses as they come in
            while let Some(result) = subscription_stream.next().await {
                match result {
                    Ok(response) => {
                        println!("Received subscription response:");
                        println!("  TXID: {}", response.txid);
                        println!("  Scripts: {:?}", response.scripts);
                        println!("  New VTXOs: {} vtxos", response.new_vtxos.len());
                        println!("  Spent VTXOs: {} vtxos", response.spent_vtxos.len());

                        if let Some(tx) = &response.tx {
                            println!("  Transaction: {}", tx.unsigned_tx.compute_txid());
                        }

                        if !response.checkpoint_txs.is_empty() {
                            println!("  Checkpoint transactions: {:?}", response.checkpoint_txs);
                        }

                        println!("---");
                    }
                    Err(e) => {
                        println!("Error receiving subscription response: {}", e);
                        break;
                    }
                }
            }

            println!("Subscription stream ended");
        }
    }

    Ok(())
}

async fn spendable_vtxos(
    grpc_client: &ark_grpc::Client,
    vtxos: &[Vtxo],
    include_recoverable_vtxos: bool,
) -> Result<HashMap<Vtxo, Vec<VirtualTxOutPoint>>> {
    let mut spendable_vtxos = HashMap::new();
    for vtxo in vtxos.iter() {
        let request = GetVtxosRequest::new_for_addresses(&[vtxo.to_ark_address()]);

        // The VTXOs for the given Ark address that the Ark server tells us about.
        let list = grpc_client.list_vtxos(request).await?;

        let spendable = if include_recoverable_vtxos {
            list.spendable_with_recoverable()
        } else {
            list.spendable().to_vec()
        };

        spendable_vtxos.insert(vtxo.clone(), spendable);
    }

    Ok(spendable_vtxos)
}

async fn settle(
    grpc_client: &ark_grpc::Client,
    server_info: &ark_core::server::Info,
    sk: SecretKey,
    vtxos: VirtualTxOutPoints,
    boarding_outputs: BoardingOutpoints,
    to_address: ArkAddress,
) -> Result<Option<Txid>> {
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    if vtxos.spendable.is_empty() && boarding_outputs.spendable.is_empty() {
        return Ok(None);
    }

    let cosigner_kp = Keypair::new(&secp, &mut rng);

    let batch_inputs = {
        let boarding_inputs = boarding_outputs.spendable.clone().into_iter().map(
            |(outpoint, amount, boarding_output)| {
                proof_of_funds::Input::new(
                    outpoint,
                    boarding_output.exit_delay(),
                    TxOut {
                        value: amount,
                        script_pubkey: boarding_output.script_pubkey(),
                    },
                    boarding_output.tapscripts(),
                    boarding_output.owner_pk(),
                    boarding_output.exit_spend_info(),
                    true,
                )
            },
        );

        let vtxo_inputs = vtxos
            .spendable
            .clone()
            .into_iter()
            .map(|(virtual_tx_outpoint, vtxo)| {
                proof_of_funds::Input::new(
                    virtual_tx_outpoint.outpoint,
                    vtxo.exit_delay(),
                    TxOut {
                        value: virtual_tx_outpoint.amount,
                        script_pubkey: vtxo.script_pubkey(),
                    },
                    vtxo.tapscripts(),
                    vtxo.owner_pk(),
                    vtxo.exit_spend_info(),
                    false,
                )
            });

        boarding_inputs.chain(vtxo_inputs).collect::<Vec<_>>()
    };
    let n_batch_inputs = batch_inputs.len();

    let spendable_amount = boarding_outputs.spendable_balance() + vtxos.spendable_balance();
    let batch_outputs = vec![proof_of_funds::Output::Offchain(TxOut {
        value: spendable_amount,
        script_pubkey: to_address.to_p2tr_script_pubkey(),
    })];

    let own_cosigner_kps = [cosigner_kp];
    let own_cosigner_pks = own_cosigner_kps
        .iter()
        .map(|k| k.public_key())
        .collect::<Vec<_>>();

    let signing_kp = Keypair::from_secret_key(&secp, &sk);
    let sign_for_onchain_pk_fn = |_: &XOnlyPublicKey,
                                  msg: &secp256k1::Message|
     -> Result<schnorr::Signature, ark_core::Error> {
        Ok(secp.sign_schnorr_no_aux_rand(msg, &signing_kp))
    };

    let signing_kp = Keypair::from_secret_key(&secp, &sk);
    let (bip322_proof, intent_message) = proof_of_funds::make_bip322_signature(
        &[signing_kp],
        sign_for_onchain_pk_fn,
        batch_inputs,
        batch_outputs,
        own_cosigner_pks.clone(),
    )?;

    let intent_id = grpc_client
        .register_intent(&intent_message, &bip322_proof)
        .await?;

    tracing::info!(intent_id, "Registered intent");

    let topics = vtxos
        .spendable
        .iter()
        .map(|(o, _)| o.outpoint.to_string())
        .chain(
            own_cosigner_pks
                .iter()
                .map(|pk| pk.serialize().to_lower_hex_string()),
        )
        .collect();

    let mut event_stream = grpc_client.get_event_stream(topics).await?;

    let mut vtxo_graph_chunks = Vec::new();

    let batch_started_event = match event_stream.next().await {
        Some(Ok(StreamEvent::BatchStarted(e))) => e,
        other => bail!("Did not get batch signing event: {other:?}"),
    };

    let hash = sha256::Hash::hash(intent_id.as_bytes());
    let hash = hash.as_byte_array().to_vec().to_lower_hex_string();

    if batch_started_event
        .intent_id_hashes
        .iter()
        .any(|h| h == &hash)
    {
        grpc_client.confirm_registration(intent_id.clone()).await?;
    } else {
        bail!(
            "Did not find intent ID {} in batch: {}",
            intent_id,
            batch_started_event.id
        )
    }

    let batch_signing_event;
    loop {
        match event_stream.next().await {
            Some(Ok(StreamEvent::TreeTx(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => vtxo_graph_chunks.push(e.tx_graph_chunk),
                BatchTreeEventType::Connector => {
                    bail!("Unexpected connector batch tree event");
                }
            },
            Some(Ok(StreamEvent::TreeSigningStarted(e))) => {
                batch_signing_event = e;
                break;
            }
            other => bail!("Unexpected event while waiting for batch signing: {other:?}"),
        }
    }

    let mut vtxo_graph = TxGraph::new(vtxo_graph_chunks)?;

    let batch_id = batch_signing_event.id;
    tracing::info!(batch_id, "Batch signing started");

    let nonce_tree = generate_nonce_tree(
        &mut rng,
        &vtxo_graph,
        cosigner_kp.public_key(),
        &batch_signing_event.unsigned_commitment_tx,
    )?;

    grpc_client
        .submit_tree_nonces(
            &batch_id,
            cosigner_kp.public_key(),
            nonce_tree.to_nonce_pks(),
        )
        .await?;

    let batch_signing_nonces_generated_event = match event_stream.next().await {
        Some(Ok(StreamEvent::TreeNoncesAggregated(e))) => e,
        other => bail!("Did not get batch signing nonces generated event: {other:?}"),
    };

    let batch_id = batch_signing_nonces_generated_event.id;

    let agg_pub_nonce_tree = batch_signing_nonces_generated_event.tree_nonces;

    tracing::info!(batch_id, "Batch combined nonces generated");

    let partial_sig_tree = sign_batch_tree(
        server_info.vtxo_tree_expiry,
        server_info.pk.x_only_public_key().0,
        &cosigner_kp,
        &vtxo_graph,
        &batch_signing_event.unsigned_commitment_tx,
        nonce_tree,
        &agg_pub_nonce_tree,
    )?;

    grpc_client
        .submit_tree_signatures(&batch_id, cosigner_kp.public_key(), partial_sig_tree)
        .await?;

    let mut connectors_graph_chunks = Vec::new();

    let batch_finalization_event;
    loop {
        match event_stream.next().await {
            Some(Ok(StreamEvent::TreeTx(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => {
                    bail!("Unexpected VTXO batch tree event");
                }
                BatchTreeEventType::Connector => {
                    connectors_graph_chunks.push(e.tx_graph_chunk);
                }
            },
            Some(Ok(StreamEvent::TreeSignature(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => {
                    vtxo_graph.apply(|graph| {
                        if graph.root().unsigned_tx.compute_txid() != e.txid {
                            Ok(true)
                        } else {
                            graph.set_signature(e.signature);

                            Ok(false)
                        }
                    })?;
                }
                BatchTreeEventType::Connector => {
                    bail!("received batch tree signature for connectors tree");
                }
            },
            Some(Ok(StreamEvent::BatchFinalization(e))) => {
                batch_finalization_event = e;
                break;
            }
            other => bail!("Unexpected event while waiting for batch finalization: {other:?}"),
        }
    }

    let batch_id = batch_finalization_event.id;

    tracing::info!(batch_id, "Batch finalization started");

    let vtxo_inputs = vtxos
        .spendable
        .into_iter()
        .map(|(outpoint, vtxo)| {
            batch::VtxoInput::new(
                vtxo,
                outpoint.amount,
                outpoint.outpoint,
                outpoint.is_recoverable(),
            )
        })
        .collect::<Vec<_>>();

    let signed_forfeit_psbts = if !vtxo_inputs.is_empty() {
        let connectors_graph = TxGraph::new(connectors_graph_chunks)?;

        create_and_sign_forfeit_txs(
            vtxo_inputs.as_slice(),
            &connectors_graph.leaves(),
            &server_info.forfeit_address,
            server_info.dust,
            |msg, _vtxo| {
                let sig = secp.sign_schnorr_no_aux_rand(msg, &signing_kp);
                let pk = signing_kp.x_only_public_key().0;
                (sig, pk)
            },
        )?
    } else {
        Vec::new()
    };

    let onchain_inputs = boarding_outputs
        .spendable
        .into_iter()
        .map(|(outpoint, amount, boarding_output)| {
            batch::OnChainInput::new(boarding_output, amount, outpoint)
        })
        .collect::<Vec<_>>();

    let commitment_psbt = if n_batch_inputs == 0 {
        None
    } else {
        let mut commitment_psbt = batch_finalization_event.commitment_tx;

        let sign_for_pk_fn = |_: &XOnlyPublicKey,
                              msg: &secp256k1::Message|
         -> Result<schnorr::Signature, ark_core::Error> {
            Ok(secp.sign_schnorr_no_aux_rand(msg, &signing_kp))
        };

        sign_commitment_psbt(sign_for_pk_fn, &mut commitment_psbt, &onchain_inputs)?;

        Some(commitment_psbt)
    };

    grpc_client
        .submit_signed_forfeit_txs(signed_forfeit_psbts, commitment_psbt)
        .await?;

    let batch_finalized_event = match event_stream.next().await {
        Some(Ok(StreamEvent::BatchFinalized(e))) => e,
        other => bail!("Did not get batch finalized event: {other:?}"),
    };

    let batch_id = batch_finalized_event.id;

    tracing::info!(batch_id, "Batch finalized");

    Ok(Some(batch_finalized_event.commitment_txid))
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
) -> Result<Vec<history::Transaction>> {
    let mut boarding_transactions = Vec::new();
    let mut boarding_commitment_transactions = Vec::new();

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

            boarding_transactions.push(history::Transaction::Boarding {
                txid: outpoint.txid,
                amount: *amount,
                confirmed_at,
            });

            let status = onchain_explorer
                .get_output_status(&outpoint.txid, outpoint.vout)
                .await?;

            if let Some(spend_txid) = status.spend_txid {
                boarding_commitment_transactions.push(spend_txid);
            }
        }
    }

    let runtime = tokio::runtime::Handle::current();

    let mut incoming_transactions = Vec::new();
    let mut outgoing_transactions = Vec::new();
    for vtxo in vtxos.iter() {
        let request = GetVtxosRequest::new_for_addresses(&[vtxo.to_ark_address()]);
        let vtxo_list = grpc_client.list_vtxos(request).await?;

        let mut new_incoming_transactions = generate_incoming_vtxo_transaction_history(
            vtxo_list.spent(),
            vtxo_list.spendable(),
            &boarding_commitment_transactions,
        )?;

        incoming_transactions.append(&mut new_incoming_transactions);

        let mut new_outgoing_transactions = generate_outgoing_vtxo_transaction_history(
            vtxo_list.spent(),
            vtxo_list.spendable(),
            |outpoint: OutPoint| {
                block_in_place(|| {
                    runtime.block_on(async {
                        let request = GetVtxosRequest::new_for_outpoints(&[outpoint]);
                        let list = grpc_client
                            .list_vtxos(request)
                            .await
                            .map_err(ark_core::Error::ad_hoc)?;

                        Ok(list.all().first().cloned())
                    })
                })
            },
        )?;

        outgoing_transactions.append(&mut new_outgoing_transactions);
    }

    let mut txs = [
        boarding_transactions,
        incoming_transactions,
        outgoing_transactions,
    ]
    .concat();

    sort_transactions_by_created_at(&mut txs);

    Ok(txs)
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
