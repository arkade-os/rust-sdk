use crate::error::ErrorContext;
use crate::key_provider::KeypairIndex;
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
use ark_core::ExplorerUtxo;
use ark_core::UtxoCoinSelection;
use ark_core::Vtxo;
use ark_core::VtxoList;
use ark_core::DEFAULT_DERIVATION_PATH;
use ark_grpc::VtxoChainResponse;
use bitcoin::bip32::DerivationPath;
use bitcoin::bip32::Xpriv;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use futures::Future;
use futures::Stream;
use std::collections::HashMap;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub mod error;
pub mod key_provider;
pub mod swap_storage;
pub mod wallet;

mod batch;
mod boltz;
mod coin_select;
mod fee_estimation;
mod send_vtxo;
mod unilateral_exit;
mod utils;

pub use boltz::ReverseSwapData;
pub use boltz::SubmarineSwapData;
pub use boltz::SwapAmount;
pub use boltz::TimeoutBlockHeights;
pub use error::Error;
pub use key_provider::Bip32KeyProvider;
pub use key_provider::KeyProvider;
pub use key_provider::StaticKeyProvider;
pub use lightning_invoice;
pub use swap_storage::InMemorySwapStorage;
#[cfg(feature = "sqlite")]
pub use swap_storage::SqliteSwapStorage;
pub use swap_storage::SwapStorage;

/// Default gap limit for BIP44-style key discovery
///
/// This is the number of consecutive unused addresses to scan before
/// assuming all used addresses have been found.
pub const DEFAULT_GAP_LIMIT: u32 = 20;

/// A client to interact with Ark Server
///
/// ## Example
///
/// ```rust
/// # use std::future::Future;
/// # use std::str::FromStr;
/// # use std::time::Duration;
/// # use ark_client::{Blockchain, Client, Error, SpendStatus, TxStatus};
/// # use ark_client::OfflineClient;
/// # use bitcoin::key::Keypair;
/// # use bitcoin::secp256k1::{Message, SecretKey};
/// # use std::sync::Arc;
/// # use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid, XOnlyPublicKey};
/// # use bitcoin::secp256k1::schnorr::Signature;
/// # use ark_client::wallet::{Balance, BoardingWallet, OnchainWallet, Persistence};
/// # use ark_client::InMemorySwapStorage;
/// # use ark_core::{BoardingOutput, UtxoCoinSelection, ExplorerUtxo};
/// # use ark_client::StaticKeyProvider;
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
/// #     async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
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
/// // Initialize the client with a static keypair
/// async fn init_client_with_keypair() -> Result<Client<MyBlockchain, MyWallet, InMemorySwapStorage, ark_client::StaticKeyProvider>, ark_client::Error> {
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
///     // Create the offline client (backward compatible method)
///     let offline_client = OfflineClient::<MyBlockchain, MyWallet, InMemorySwapStorage, StaticKeyProvider>::new_with_keypair(
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
///
/// // Initialize the client with a BIP32 HD wallet
/// # use bitcoin::bip32::{Xpriv, DerivationPath};
/// async fn init_client_with_bip32() -> Result<Client<MyBlockchain, MyWallet, InMemorySwapStorage, ark_client::Bip32KeyProvider>, ark_client::Error> {
///     // Create a BIP32 master key and derivation path
///     let master_key = Xpriv::from_str("xprv...").unwrap();
///     let derivation_path = DerivationPath::from_str("m/84'/0'/0'/0/0").unwrap();
///
///     let key_provider = Arc::new(ark_client::Bip32KeyProvider::new(master_key, derivation_path));
///
///     // Initialize blockchain and wallet implementations
///     let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
///     let wallet = Arc::new(MyWallet {});
///     let timeout = Duration::from_secs(30);
///
///     // Create the offline client with BIP32 key provider
///     let offline_client = OfflineClient::new(
///         "my-ark-client".to_string(),
///         key_provider,
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
pub struct OfflineClient<B, W, S, K> {
    // TODO: We could introduce a generic interface so that consumers can use either GRPC or REST.
    network_client: ark_grpc::Client,
    pub name: String,
    key_provider: Arc<K>,
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
pub struct Client<B, W, S, K> {
    inner: OfflineClient<B, W, S, K>,
    pub server_info: server::Info,
}

#[derive(Clone, Copy, Debug)]
pub struct TxStatus {
    pub confirmed_at: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SpendStatus {
    pub spend_txid: Option<Txid>,
}

pub struct AddressVtxos {
    pub unspent: Vec<VirtualTxOutPoint>,
    pub spent: Vec<VirtualTxOutPoint>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OffChainBalance {
    pre_confirmed: Amount,
    confirmed: Amount,
    recoverable: Amount,
}

impl OffChainBalance {
    pub fn pre_confirmed(&self) -> Amount {
        self.pre_confirmed
    }

    pub fn confirmed(&self) -> Amount {
        self.confirmed
    }

    /// Balance which can only be settled, and does not require a forfeit transaction per VTXO.
    pub fn recoverable(&self) -> Amount {
        self.recoverable
    }

    pub fn total(&self) -> Amount {
        self.pre_confirmed + self.confirmed + self.recoverable
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

    fn get_tx_status(&self, txid: &Txid) -> impl Future<Output = Result<TxStatus, Error>> + Send;

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

impl<B, W, S, K> OfflineClient<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: KeyProvider,
{
    /// Create a new offline client with a generic key provider
    ///
    /// # Arguments
    ///
    /// * `name` - Client identifier
    /// * `key_provider` - Implementation of KeyProvider trait (StaticKeyProvider, Bip32KeyProvider,
    ///   etc.)
    /// * `blockchain` - Blockchain interface implementation
    /// * `wallet` - Wallet implementation
    /// * `ark_server_url` - URL of the Ark server
    /// * `swap_storage` - Storage implementation for swap data
    /// * `boltz_url` - URL of the Boltz server
    /// * `timeout` - Timeout duration for network operations
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        key_provider: Arc<K>,
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
            key_provider,
            blockchain,
            secp,
            wallet,
            swap_storage,
            boltz_url,
            timeout,
        }
    }

    /// Create a new offline client with a static keypair (backward compatible)
    ///
    /// This is a convenience method that wraps a single keypair in a StaticKeyProvider.
    ///
    /// # Arguments
    ///
    /// * `name` - Client identifier
    /// * `kp` - Static keypair for signing
    /// * `blockchain` - Blockchain interface implementation
    /// * `wallet` - Wallet implementation
    /// * `ark_server_url` - URL of the Ark server
    /// * `swap_storage` - Storage implementation for swap data
    /// * `boltz_url` - URL of the Boltz server
    /// * `timeout` - Timeout duration for network operations
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_keypair(
        name: String,
        kp: Keypair,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        ark_server_url: String,
        swap_storage: Arc<S>,
        boltz_url: String,
        timeout: Duration,
    ) -> OfflineClient<B, W, S, StaticKeyProvider> {
        let key_provider = Arc::new(StaticKeyProvider::new(kp));

        OfflineClient::new(
            name,
            key_provider,
            blockchain,
            wallet,
            ark_server_url,
            swap_storage,
            boltz_url,
            timeout,
        )
    }

    /// Create a new offline client with an [`Xpriv`]
    ///
    /// # Arguments
    ///
    /// * `name` - Client identifier
    /// * `xpriv` - BIP32 Xpriv
    /// * `blockchain` - Blockchain interface implementation
    /// * `wallet` - Wallet implementation
    /// * `ark_server_url` - URL of the Ark server
    /// * `swap_storage` - Storage implementation for swap data
    /// * `boltz_url` - URL of the Boltz server
    /// * `timeout` - Timeout duration for network operations
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_bip32(
        name: String,
        xpriv: Xpriv,
        path: Option<DerivationPath>,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        ark_server_url: String,
        swap_storage: Arc<S>,
        boltz_url: String,
        timeout: Duration,
    ) -> OfflineClient<B, W, S, Bip32KeyProvider> {
        let path = path.unwrap_or(
            DerivationPath::from_str(DEFAULT_DERIVATION_PATH).expect("valid derivation path"),
        );
        let key_provider = Arc::new(Bip32KeyProvider::new(xpriv, path));

        OfflineClient::new(
            name,
            key_provider,
            blockchain,
            wallet,
            ark_server_url,
            swap_storage,
            boltz_url,
            timeout,
        )
    }

    /// Connects to the Ark server and retrieves server information.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or times out.
    pub async fn connect(mut self) -> Result<Client<B, W, S, K>, Error> {
        timeout_op(self.timeout, self.network_client.connect())
            .await
            .context("Failed to connect to Ark server")??;

        self.finish_connect().await
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
    ) -> Result<Client<B, W, S, K>, Error> {
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

        self.finish_connect().await
    }

    async fn finish_connect(mut self) -> Result<Client<B, W, S, K>, Error> {
        let server_info = timeout_op(self.timeout, self.network_client.get_info())
            .await
            .context("Failed to get Ark server info")??;

        tracing::debug!(
            name = self.name,
            ark_server_url = ?self.network_client,
            "Connected to Ark server"
        );

        let client = Client {
            inner: self,
            server_info,
        };

        if let Err(error) = client.discover_keys(DEFAULT_GAP_LIMIT).await {
            tracing::warn!(?error, "Failed during key discovery");
        };

        if let Err(error) = client.continue_pending_offchain_txs().await {
            tracing::warn!(?error, "Failed to recover pending transactions");
        };

        Ok(client)
    }
}

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: KeyProvider,
{
    /// Get a new offchain receiving address
    ///
    /// For HD wallets, this will derive a new address each time it's called.
    /// For static key providers, this will always return the same address.
    pub fn get_offchain_address(&self) -> Result<(ArkAddress, Vtxo), Error> {
        let server_info = &self.server_info;

        let server_signer = server_info.signer_pk.into();
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .public_key()
            .into();

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
        let server_info = &self.server_info;
        let server_signer = server_info.signer_pk.into();

        let pks = self.inner.key_provider.get_cached_pks()?;

        pks.into_iter()
            .map(|owner_pk| {
                let vtxo = Vtxo::new_default(
                    self.secp(),
                    server_signer,
                    owner_pk,
                    server_info.unilateral_exit_delay,
                    server_info.network,
                )?;

                let ark_address = vtxo.to_ark_address();

                Ok((ark_address, vtxo))
            })
            .collect::<Result<Vec<_>, _>>()
    }

    /// Discover and cache used keys using BIP44-style gap limit
    ///
    /// This method derives keys in batches, checks all at once via list_vtxos,
    /// caches used ones, and stops when a full batch has no used keys.
    ///
    /// Returns the number of discovered keys. No-op for StaticKeyProvider.
    ///
    /// # Arguments
    ///
    /// * `gap_limit` - Number of consecutive unused addresses before stopping
    pub async fn discover_keys(&self, gap_limit: u32) -> Result<u32, Error> {
        if !self.inner.key_provider.supports_discovery() {
            tracing::debug!("Key provider does not support discovery, skipping");
            return Ok(0);
        }

        let server_info = &self.server_info;
        let server_signer: XOnlyPublicKey = server_info.signer_pk.into();

        let mut start_index = 0u32;
        let mut discovered_count = 0u32;

        tracing::info!(gap_limit, "Starting key discovery");

        loop {
            // Generate a batch of gap_limit keys
            let mut batch: Vec<(u32, Keypair, ArkAddress)> = Vec::with_capacity(gap_limit as usize);

            for i in 0..gap_limit {
                let index = start_index
                    .checked_add(i)
                    .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;

                let kp = match self.inner.key_provider.derive_at_discovery_index(index)? {
                    Some(kp) => kp,
                    None => break,
                };

                let vtxo = Vtxo::new_default(
                    self.secp(),
                    server_signer,
                    kp.x_only_public_key().0,
                    server_info.unilateral_exit_delay,
                    server_info.network,
                )?;

                batch.push((index, kp, vtxo.to_ark_address()));
            }

            if batch.is_empty() {
                break;
            }

            // Query all addresses in batch at once
            let addresses = batch.iter().map(|(_, _, a)| *a);

            let vtxo_list = self.list_vtxos_for_addresses(addresses).await?;

            // Build set of used scripts from response
            let used_scripts: HashSet<&ScriptBuf> = vtxo_list.all().map(|v| &v.script).collect();

            // Cache keypairs for used addresses (match by script)
            let mut found_any = false;
            for (index, kp, addr) in batch {
                let script = addr.to_p2tr_script_pubkey();
                if used_scripts.contains(&script) {
                    tracing::debug!(index, %addr, "Found used address");
                    self.inner
                        .key_provider
                        .cache_discovered_keypair(index, kp)?;
                    discovered_count += 1;
                    found_any = true;
                }
            }

            // Stop if no used addresses found in this batch (gap limit reached)
            if !found_any {
                break;
            }

            start_index = start_index
                .checked_add(gap_limit)
                .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;
        }

        tracing::info!(discovered_count, "Key discovery completed");

        Ok(discovered_count)
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

    pub async fn get_virtual_tx_outpoints(
        &self,
        addresses: impl Iterator<Item = ArkAddress>,
    ) -> Result<Vec<VirtualTxOutPoint>, Error> {
        let request = GetVtxosRequest::new_for_addresses(addresses);
        self.fetch_all_vtxos(request).await
    }

    pub async fn list_vtxos(&self) -> Result<(VtxoList, HashMap<ScriptBuf, Vtxo>), Error> {
        let ark_addresses = self.get_offchain_addresses()?;

        let script_pubkey_to_vtxo_map = ark_addresses
            .iter()
            .map(|(a, v)| (a.to_p2tr_script_pubkey(), v.clone()))
            .collect();

        let addresses = ark_addresses.iter().map(|(a, _)| a).copied();

        let vtxo_list = self.list_vtxos_for_addresses(addresses).await?;

        Ok((vtxo_list, script_pubkey_to_vtxo_map))
    }

    pub async fn list_vtxos_for_addresses(
        &self,
        addresses: impl Iterator<Item = ArkAddress>,
    ) -> Result<VtxoList, Error> {
        let virtual_tx_outpoints = self
            .get_virtual_tx_outpoints(addresses)
            .await
            .context("failed to get VTXOs for addresses")?;

        let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

        Ok(vtxo_list)
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

    pub async fn offchain_balance(&self) -> Result<OffChainBalance, Error> {
        let (vtxo_list, _) = self.list_vtxos().await.context("failed to list VTXOs")?;

        let pre_confirmed = vtxo_list
            .pre_confirmed()
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        let confirmed = vtxo_list
            .confirmed()
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        let recoverable = vtxo_list
            .recoverable()
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        Ok(OffChainBalance {
            pre_confirmed,
            confirmed,
            recoverable,
        })
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

        let (vtxo_list, _) = self.list_vtxos().await?;

        let spent_outpoints = vtxo_list.spent().cloned().collect::<Vec<_>>();
        let unspent_outpoints = vtxo_list.all_unspent().cloned().collect::<Vec<_>>();

        let incoming_transactions = generate_incoming_vtxo_transaction_history(
            &spent_outpoints,
            &unspent_outpoints,
            &boarding_commitment_transactions,
        )?;

        let outgoing_txs =
            generate_outgoing_vtxo_transaction_history(&spent_outpoints, &unspent_outpoints)?;

        let mut outgoing_transactions = vec![];
        for tx in outgoing_txs {
            let tx = match tx {
                OutgoingTransaction::Complete(tx) => tx,
                OutgoingTransaction::Incomplete(incomplete_tx) => {
                    let first_outpoint = incomplete_tx.first_outpoint();

                    let request = GetVtxosRequest::new_for_outpoints(&[first_outpoint]);
                    let vtxos = self.fetch_all_vtxos(request).await?;

                    match vtxos.first() {
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
                OutgoingTransaction::IncompleteOffboard(incomplete_offboard) => {
                    let status = timeout_op(
                        self.inner.timeout,
                        self.blockchain()
                            .get_tx_status(&incomplete_offboard.commitment_txid()),
                    )
                    .await
                    .context("failed to get commitment TX status")??;

                    incomplete_offboard.finish(status.confirmed_at)
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

    pub fn network_client(&self) -> ark_grpc::Client {
        self.inner.network_client.clone()
    }

    /// Fetch all VTXOs for a request, handling pagination internally.
    async fn fetch_all_vtxos(
        &self,
        request: GetVtxosRequest,
    ) -> Result<Vec<VirtualTxOutPoint>, Error> {
        if request.reference().is_empty() {
            return Ok(Vec::new());
        }

        let mut all_vtxos = Vec::new();
        let mut cursor = 0;
        const PAGE_SIZE: i32 = 100;

        loop {
            let paged_request = request.clone().with_page(PAGE_SIZE, cursor);
            let response = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(paged_request),
            )
            .await
            .context("failed to fetch list of VTXOs")??;

            all_vtxos.extend(response.vtxos);

            // Use server-provided cursor for next page; next == total means end
            match response.page {
                Some(page) if page.next < page.total => {
                    cursor = page.next;
                }
                _ => break,
            }
        }

        Ok(all_vtxos)
    }

    fn next_keypair(&self, keypair_index: KeypairIndex) -> Result<Keypair, Error> {
        self.inner.key_provider.get_next_keypair(keypair_index)
    }
    fn keypair_by_pk(&self, pk: &XOnlyPublicKey) -> Result<Keypair, Error> {
        self.inner.key_provider.get_keypair_for_pk(pk)
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
