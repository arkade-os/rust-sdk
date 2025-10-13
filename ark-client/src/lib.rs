use crate::error::ErrorContext;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use ark_core::build_anchor_tx;
use ark_core::history;
use ark_core::history::generate_incoming_vtxo_transaction_history;
use ark_core::history::generate_outgoing_vtxo_transaction_history;
use ark_core::history::sort_transactions_by_created_at;
use ark_core::history::OutgoingTransaction;
use ark_core::server;
use ark_core::server::GetVtxosRequest;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkAddress;
use ark_core::UtxoCoinSelection;
use ark_core::Vtxo;
use ark_grpc::VtxoChainResponse;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Transaction;
use bitcoin::Txid;
use futures::Future;
use futures::Stream;
use jiff::Timestamp;
use std::sync::Arc;
use std::time::Duration;

pub mod error;
pub mod swap_storage;
pub mod wallet;

mod batch;
mod boltz;
mod coin_select;
mod send_vtxo;
mod unilateral_exit;
mod utils;

pub use error::Error;
pub use lightning_invoice;
pub use swap_storage::InMemorySwapStorage;
pub use swap_storage::SqliteSwapStorage;
pub use swap_storage::SwapStorage;

/// A client to interact with Ark Server
///
/// ## Example
///
/// ```rust
/// # use std::future::Future;
/// # use std::str::FromStr;
/// # use std::time::Duration;
/// # use ark_client::{Blockchain, Client, Error, ExplorerUtxo, SpendStatus};
/// # use ark_client::OfflineClient;
/// # use bitcoin::key::Keypair;
/// # use bitcoin::secp256k1::{Message, SecretKey};
/// # use std::sync::Arc;
/// # use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid, XOnlyPublicKey};
/// # use bitcoin::secp256k1::schnorr::Signature;
/// # use ark_client::wallet::{Balance, BoardingWallet, OnchainWallet, Persistence};
/// # use ark_client::InMemorySwapStorage;
/// # use ark_core::{BoardingOutput, UtxoCoinSelection};
///
/// struct MyBlockchain {}
/// #
/// # impl MyBlockchain {
/// #     pub fn new(_url: &str) -> Self { Self {}}
/// # }
/// #
/// # impl Blockchain for MyBlockchain {
/// #
/// #     async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
/// #         unimplemented!("You can implement this function using your preferred client library such as esplora_client")
/// #     }
/// #
/// #     async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     async fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     async fn get_fee_rate(&self) -> Result<f64, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     async fn broadcast_package(
/// #         &self,
/// #         txs: &[&Transaction],
/// #     ) -> Result<(), Error> {
/// #         unimplemented!()
/// #     }
/// # }
///
/// struct MyWallet {}
/// # impl OnchainWallet for MyWallet where {
/// #
/// #     fn get_onchain_address(&self) -> Result<Address, Error> {
/// #         unimplemented!("You can implement this function using your preferred client library such as bdk")
/// #     }
/// #
/// #     async fn sync(&self) -> Result<(), Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn balance(&self) -> Result<Balance, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn prepare_send_to_address(&self, address: Address, amount: Amount, fee_rate: FeeRate) -> Result<Psbt, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn sign(&self, psbt: &mut Psbt) -> Result<bool, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn select_coins(&self, target_amount: Amount) -> Result<ark_core::UtxoCoinSelection, Error> {
/// #         unimplemented!()
/// #     }
/// # }
/// #
///
/// struct InMemoryDb {}
/// # impl Persistence for InMemoryDb {
/// #
/// #     fn save_boarding_output(
/// #         &self,
/// #         sk: SecretKey,
/// #         boarding_output: BoardingOutput,
/// #     ) -> Result<(), Error> {
/// #       unimplemented!()
/// #     }
/// #
/// #     fn load_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, Error> {
/// #           unimplemented!()
/// #     }
/// #
/// #     fn sk_for_pk(&self, pk: &XOnlyPublicKey) -> Result<SecretKey, Error> {
/// #         unimplemented!()
/// #     }
/// # }
/// #
/// #
/// # impl BoardingWallet for MyWallet
/// # where
/// # {
/// #     fn new_boarding_output(
/// #         &self,
/// #         server_pk: XOnlyPublicKey,
/// #         exit_delay: bitcoin::Sequence,
/// #         network: Network,
/// #     ) -> Result<BoardingOutput, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn get_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, Error> {
/// #         unimplemented!()
/// #     }
/// #
/// #     fn sign_for_pk(&self, pk: &XOnlyPublicKey, msg: &Message) -> Result<Signature, Error> {
/// #         unimplemented!()
/// #     }
/// # }
/// #
/// // Initialize the client
/// async fn init_client() -> Result<Client<MyBlockchain, MyWallet, InMemorySwapStorage>, ark_client::Error> {
///     // Create a keypair for signing transactions
///     let secp = bitcoin::key::Secp256k1::new();
///     let secret_key = SecretKey::from_str("your_private_key_here").unwrap();
///     let keypair = Keypair::from_secret_key(&secp, &secret_key);
///
///     // Initialize blockchain and wallet implementations
///     let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
///     let wallet = Arc::new(MyWallet {});
///     let timeout = Duration::from_secs(30);
///
///     // Create the offline client
///     let offline_client = OfflineClient::new(
///         "my-ark-client".to_string(),
///         keypair,
///         blockchain,
///         wallet,
///         "https://ark-server.example.com".to_string(),
///         Arc::new(InMemorySwapStorage::default()),
///         "http://boltz.example.com".to_string(),
///         timeout
///     );
///
///     // Connect to the Ark server and get server info
///     let client = offline_client.connect().await?;
///
///     Ok(client)
/// }
/// ```
#[derive(Clone)]
pub struct OfflineClient<B, W, S> {
    // TODO: We could introduce a generic interface so that consumers can use either GRPC or REST.
    network_client: ark_grpc::Client,
    pub name: String,
    pub kp: Keypair,
    blockchain: Arc<B>,
    secp: Secp256k1<All>,
    wallet: Arc<W>,
    swap_storage: Arc<S>,
    boltz_url: String,
    timeout: Duration,
}

/// A client to interact with Ark server
///
/// See [`OfflineClient`] docs for details.
pub struct Client<B, W, S> {
    inner: OfflineClient<B, W, S>,
    pub server_info: server::Info,
}

#[derive(Clone, Copy, Debug)]
pub struct ExplorerUtxo {
    pub outpoint: OutPoint,
    pub amount: Amount,
    pub confirmation_blocktime: Option<u64>,
    pub is_spent: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SpendStatus {
    pub spend_txid: Option<Txid>,
}

#[derive(Clone, Debug, Default)]
pub struct ListVtxo {
    pub spendable: Vec<(Vec<VirtualTxOutPoint>, Vtxo)>,
    pub spent: Vec<(Vec<VirtualTxOutPoint>, Vtxo)>,
}
impl ListVtxo {
    fn spendable_outpoints(&self) -> Vec<VirtualTxOutPoint> {
        self.spendable
            .iter()
            .flat_map(|(os, _)| os.clone())
            .collect()
    }

    fn spent_outpoints(&self) -> Vec<VirtualTxOutPoint> {
        self.spent.iter().flat_map(|(os, _)| os.clone()).collect()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OffChainBalance {
    pending: Amount,
    confirmed: Amount,
}

impl OffChainBalance {
    pub fn pending(&self) -> Amount {
        self.pending
    }

    pub fn confirmed(&self) -> Amount {
        self.confirmed
    }

    pub fn total(&self) -> Amount {
        self.pending + self.confirmed
    }
}

pub trait Blockchain {
    fn find_outpoints(
        &self,
        address: &Address,
    ) -> impl Future<Output = Result<Vec<ExplorerUtxo>, Error>> + Send;

    fn find_tx(
        &self,
        txid: &Txid,
    ) -> impl Future<Output = Result<Option<Transaction>, Error>> + Send;

    fn get_output_status(
        &self,
        txid: &Txid,
        vout: u32,
    ) -> impl Future<Output = Result<SpendStatus, Error>> + Send;

    fn broadcast(&self, tx: &Transaction) -> impl Future<Output = Result<(), Error>> + Send;

    fn get_fee_rate(&self) -> impl Future<Output = Result<f64, Error>> + Send;

    fn broadcast_package(
        &self,
        txs: &[&Transaction],
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

impl<B, W, S> OfflineClient<B, W, S>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        kp: Keypair,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        ark_server_url: String,
        swap_storage: Arc<S>,
        boltz_url: String,
        timeout: Duration,
    ) -> Self {
        let secp = Secp256k1::new();

        let network_client = ark_grpc::Client::new(ark_server_url);

        Self {
            network_client,
            name,
            kp,
            blockchain,
            secp,
            wallet,
            swap_storage,
            boltz_url,
            timeout,
        }
    }

    /// Connects to the Ark server and retrieves server information.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or times out.
    pub async fn connect(mut self) -> Result<Client<B, W, S>, Error> {
        timeout_op(self.timeout, self.network_client.connect())
            .await
            .context("Failed to connect to Ark server")??;
        let server_info = timeout_op(self.timeout, self.network_client.get_info())
            .await
            .context("Failed to get Ark server info")??;

        tracing::debug!(
            name = self.name,
            ark_server_url = ?self.network_client,
            "Connected to Ark server"
        );

        Ok(Client {
            inner: self,
            server_info,
        })
    }

    /// Connects to the Ark server and retrieves server information.
    ///
    /// If it encounters errors, it will retry `max_retries`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or times out.
    pub async fn connect_with_retries(
        mut self,
        max_retries: usize,
    ) -> Result<Client<B, W, S>, Error> {
        let mut n_retries = 0;
        while n_retries < max_retries {
            let res = timeout_op(self.timeout, self.network_client.connect())
                .await
                .context("Failed to connect to Ark server")?;

            match res {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(?error, "Failed to connect to Ark server, retrying");

                    sleep(Duration::from_secs(2)).await;

                    n_retries += 1;

                    continue;
                }
            };
        }

        let server_info = timeout_op(self.timeout, self.network_client.get_info())
            .await
            .context("Failed to get Ark server info")??;

        tracing::debug!(
            name = self.name,
            ark_server_url = ?self.network_client,
            "Connected to Ark server"
        );

        Ok(Client {
            inner: self,
            server_info,
        })
    }
}

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
{
    // At the moment we are always generating the same address.
    pub fn get_offchain_address(&self) -> Result<(ArkAddress, Vtxo), Error> {
        let server_info = &self.server_info;

        let server_signer = server_info.signer_pk.into();
        let owner = self.inner.kp.public_key().into();

        let vtxo = Vtxo::new_default(
            self.secp(),
            server_signer,
            owner,
            server_info.unilateral_exit_delay,
            server_info.network,
        )?;

        let ark_address = vtxo.to_ark_address();

        Ok((ark_address, vtxo))
    }

    pub fn get_offchain_addresses(&self) -> Result<Vec<(ArkAddress, Vtxo)>, Error> {
        let address = self.get_offchain_address()?;

        Ok(vec![address])
    }

    // At the moment we are always generating the same address.
    pub fn get_boarding_address(&self) -> Result<Address, Error> {
        let server_info = &self.server_info;

        let boarding_output = self.inner.wallet.new_boarding_output(
            server_info.signer_pk.into(),
            server_info.boarding_exit_delay,
            server_info.network,
        )?;

        Ok(boarding_output.address().clone())
    }

    pub fn get_onchain_address(&self) -> Result<Address, Error> {
        self.inner.wallet.get_onchain_address()
    }

    pub fn get_boarding_addresses(&self) -> Result<Vec<Address>, Error> {
        let address = self.get_boarding_address()?;

        Ok(vec![address])
    }

    pub async fn list_vtxos(&self, include_recoverable_vtxos: bool) -> Result<ListVtxo, Error> {
        let addresses = self.get_offchain_addresses()?;

        let mut vtxos = ListVtxo {
            spendable: Vec::new(),
            spent: Vec::new(),
        };

        for (address, vtxo) in addresses.into_iter() {
            let request = GetVtxosRequest::new_for_addresses(&[address]);

            let list = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(request),
            )
            .await
            .context("Failed to fetch list of VTXOs")??;

            if include_recoverable_vtxos {
                vtxos
                    .spent
                    .push((list.spent_without_recoverable().to_vec(), vtxo.clone()));

                vtxos
                    .spendable
                    .push((list.spendable_with_recoverable().to_vec(), vtxo.clone()));
            } else {
                vtxos.spent.push((list.spent().to_vec(), vtxo.clone()));

                vtxos
                    .spendable
                    .push((list.spendable().to_vec(), vtxo.clone()));
            }
        }

        Ok(vtxos)
    }

    pub async fn get_vtxo_chain(
        &self,
        out_point: OutPoint,
        size: i32,
        index: i32,
    ) -> Result<Option<VtxoChainResponse>, Error> {
        let vtxo_chain = timeout_op(
            self.inner.timeout,
            self.network_client()
                .get_vtxo_chain(Some(out_point), Some((size, index))),
        )
        .await
        .context("Failed to fetch VTXO chain")??;

        Ok(Some(vtxo_chain))
    }

    pub async fn spendable_vtxos(
        &self,
        include_recoverable_vtxos: bool,
    ) -> Result<Vec<(Vec<VirtualTxOutPoint>, Vtxo)>, Error> {
        let now = Timestamp::now();

        let mut spendable = vec![];

        let vtxos = self.list_vtxos(include_recoverable_vtxos).await?;

        for (virtual_tx_outpoints, vtxo) in vtxos.spendable {
            let explorer_utxos = timeout_op(
                self.inner.timeout,
                self.blockchain().find_outpoints(vtxo.address()),
            )
            .await
            .context("Failed to find outpoints")??;

            let mut spendable_outpoints = Vec::new();
            for virtual_tx_outpoint in virtual_tx_outpoints {
                match explorer_utxos
                    .iter()
                    .find(|explorer_utxo| explorer_utxo.outpoint == virtual_tx_outpoint.outpoint)
                {
                    // Exclude VTXOs that have been confirmed on the blockchain, but whose exit path
                    // is now _active_. These should be claimed unilaterally instead.
                    Some(ExplorerUtxo {
                        confirmation_blocktime: Some(confirmation_blocktime),
                        ..
                    }) if vtxo.can_be_claimed_unilaterally_by_owner(
                        now.as_duration().try_into().map_err(Error::ad_hoc)?,
                        Duration::from_secs(*confirmation_blocktime),
                    ) => {}
                    // All other VTXOs are spendable.
                    _ => {
                        spendable_outpoints.push(virtual_tx_outpoint);
                    }
                }
            }

            spendable.push((spendable_outpoints, vtxo));
        }

        Ok(spendable)
    }

    pub async fn offchain_balance(&self) -> Result<OffChainBalance, Error> {
        // We should not include recoverable VTXOS in the spendable balance because they cannot be
        // spent until they are claimed.
        let list = self
            .spendable_vtxos(false)
            .await
            .context("failed to get spendable VTXOs")?;
        let sum =
            list.iter()
                .flat_map(|(vtxos, _)| vtxos)
                .fold(OffChainBalance::default(), |acc, x| {
                    match x.is_preconfirmed {
                        true => OffChainBalance {
                            pending: acc.pending + x.amount,
                            ..acc
                        },
                        false => OffChainBalance {
                            confirmed: acc.confirmed + x.amount,
                            ..acc
                        },
                    }
                });

        Ok(sum)
    }

    pub async fn transaction_history(&self) -> Result<Vec<history::Transaction>, Error> {
        let mut boarding_transactions = Vec::new();
        let mut boarding_commitment_transactions = Vec::new();

        let boarding_addresses = self.get_boarding_addresses()?;
        for boarding_address in boarding_addresses.iter() {
            let outpoints = timeout_op(
                self.inner.timeout,
                self.blockchain().find_outpoints(boarding_address),
            )
            .await
            .context("Failed to find outpoints")??;

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

                let status = timeout_op(
                    self.inner.timeout,
                    self.blockchain()
                        .get_output_status(&outpoint.txid, outpoint.vout),
                )
                .await
                .context("Failed to get Tx output status")??;

                if let Some(spend_txid) = status.spend_txid {
                    boarding_commitment_transactions.push(spend_txid);
                }
            }
        }

        let vtxos = self.list_vtxos(true).await?;

        let incoming_transactions = generate_incoming_vtxo_transaction_history(
            &vtxos.spent_outpoints(),
            &vtxos.spendable_outpoints(),
            &boarding_commitment_transactions,
        )?;

        let outgoing_txs = generate_outgoing_vtxo_transaction_history(
            &vtxos.spent_outpoints(),
            &vtxos.spendable_outpoints(),
        )?;

        let mut outgoing_transactions = vec![];
        for tx in outgoing_txs {
            let tx = match tx {
                OutgoingTransaction::Complete(tx) => tx,
                OutgoingTransaction::Incomplete(incomplete_tx) => {
                    let first_outpoint = incomplete_tx.first_outpoint();

                    let request = GetVtxosRequest::new_for_outpoints(&[first_outpoint]);
                    let list = timeout_op(
                        self.inner.timeout,
                        self.network_client().list_vtxos(request),
                    )
                    .await
                    .context("Failed to fetch list of VTXOs")??;

                    match list.all().first() {
                        Some(virtual_tx_outpoint) => {
                            match incomplete_tx.finish(virtual_tx_outpoint) {
                                Ok(tx) => tx,
                                Err(e) => {
                                    tracing::warn!(
                                        %first_outpoint,
                                        "Could not finish outgoing TX, skipping: {e}"
                                    );
                                    continue;
                                }
                            }
                        }
                        None => {
                            tracing::warn!(
                                %first_outpoint,
                                "Could not find virtual TX outpoint for outgoing TX, skipping"
                            );
                            continue;
                        }
                    }
                }
            };

            outgoing_transactions.push(tx);
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

    /// Get the boarding exit delay defined by the Ark server, in seconds.
    ///
    /// # Panics
    ///
    /// This will panic if the boarding exit delay corresponds to a relative locktime specified in
    /// blocks. We expect the Ark server to use a relative locktime in seconds.
    ///
    /// This will also panic if the sequence number returned by the server is not a valid relative
    /// locktime.
    pub fn boarding_exit_delay_seconds(&self) -> u64 {
        match self
            .server_info
            .boarding_exit_delay
            .to_relative_lock_time()
            .expect("relative locktime")
        {
            bitcoin::relative::LockTime::Time(time) => time.value() as u64 * 512,
            bitcoin::relative::LockTime::Blocks(_) => unreachable!(),
        }
    }

    /// Get the unilateral exit delay for VTXOs defined by the Ark server, in seconds.
    ///
    /// # Panics
    ///
    /// This will panic if the unilateral exit delay corresponds to a relative locktime specified in
    /// blocks. We expect the Ark server to use a relative locktime in seconds.
    ///
    /// This will also panic if the sequence number returned by the server is not a valid relative
    /// locktime.
    pub fn unilateral_vtxo_exit_delay_seconds(&self) -> u64 {
        match self
            .server_info
            .unilateral_exit_delay
            .to_relative_lock_time()
            .expect("relative locktime")
        {
            bitcoin::relative::LockTime::Time(time) => time.value() as u64 * 512,
            bitcoin::relative::LockTime::Blocks(_) => unreachable!(),
        }
    }

    fn network_client(&self) -> ark_grpc::Client {
        self.inner.network_client.clone()
    }

    fn kp(&self) -> &Keypair {
        &self.inner.kp
    }

    fn secp(&self) -> &Secp256k1<All> {
        &self.inner.secp
    }

    fn blockchain(&self) -> &B {
        &self.inner.blockchain
    }

    fn swap_storage(&self) -> &S {
        &self.inner.swap_storage
    }

    /// Use the P2A output of a transaction to bump its transaction fee with a child transaction.
    pub async fn bump_tx(&self, parent: &Transaction) -> Result<Transaction, Error> {
        let fee_rate = timeout_op(self.inner.timeout, self.blockchain().get_fee_rate())
            .await
            .context("Failed to retrieve fee rate")??;

        let change_address = self.inner.wallet.get_onchain_address()?;

        // Create a closure that converts CoinSelectionResult to UtxoCoinSelection
        let select_coins_fn =
            |target_amount: Amount| -> Result<UtxoCoinSelection, ark_core::Error> {
                self.inner.wallet.select_coins(target_amount).map_err(|e| {
                    ark_core::Error::ad_hoc(format!("failed to select coins for anchor TX: {e}"))
                })
            };

        // Build the PSBT using ark-core (includes witness UTXO setup)
        let mut psbt = build_anchor_tx(parent, change_address, fee_rate, select_coins_fn)
            .map_err(|e| Error::ad_hoc(e.to_string()))?;

        // Sign the transaction
        self.inner
            .wallet
            .sign(&mut psbt)
            .context("failed to sign bump TX")?;

        // Extract the final transaction
        let tx = psbt.extract_tx().map_err(Error::ad_hoc)?;

        Ok(tx)
    }

    /// Subscribe to receive transaction notifications for specific VTXO scripts
    ///
    /// This method allows you to subscribe to get notified about transactions
    /// affecting the provided VTXO addresses. It can also be used to update an
    /// existing subscription by adding new scripts to it.
    ///
    /// # Arguments
    ///
    /// * `scripts` - Vector of ArkAddress to subscribe to
    /// * `subscription_id` - Unique identifier for the subscription. Use the same ID to update an
    ///   existing subscription. Use None for new subscriptions
    ///
    /// # Returns
    ///
    /// Returns the subscription ID if successful
    pub async fn subscribe_to_scripts(
        &self,
        scripts: Vec<ArkAddress>,
        subscription_id: Option<String>,
    ) -> Result<String, Error> {
        self.network_client()
            .subscribe_to_scripts(scripts, subscription_id)
            .await
            .map_err(Into::into)
    }

    /// Remove scripts from an existing subscription
    ///
    /// This method allows you to unsubscribe from receiving notifications for
    /// specific VTXO scripts while keeping the subscription active for other scripts.
    ///
    /// # Arguments
    ///
    /// * `scripts` - Vector of ArkAddress to unsubscribe from
    /// * `subscription_id` - The subscription ID to update
    pub async fn unsubscribe_from_scripts(
        &self,
        scripts: Vec<ArkAddress>,
        subscription_id: String,
    ) -> Result<(), Error> {
        self.network_client()
            .unsubscribe_from_scripts(scripts, subscription_id)
            .await
            .map_err(Into::into)
    }

    /// Get a subscription stream that returns subscription responses
    ///
    /// This method returns a stream that yields SubscriptionResponse messages
    /// containing information about new and spent VTXOs for the subscribed scripts.
    ///
    /// # Arguments
    ///
    /// * `subscription_id` - The subscription ID to get the stream for
    ///
    /// # Returns
    ///
    /// Returns a Stream of SubscriptionResponse messages
    pub async fn get_subscription(
        &self,
        subscription_id: String,
    ) -> Result<impl Stream<Item = Result<SubscriptionResponse, ark_grpc::Error>> + Unpin, Error>
    {
        self.network_client()
            .get_subscription(subscription_id)
            .await
            .map_err(Into::into)
    }
}
