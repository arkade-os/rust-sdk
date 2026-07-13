use crate::error::ErrorContext;
use crate::key_provider::KeypairIndex;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::utils::unix_now;
use crate::wallet::OnchainWallet;
use ark_core::asset::AssetId;
use ark_core::build_anchor_tx;
use ark_core::contract::BoardingContract;
use ark_core::contract::ContractContext;
use ark_core::contract::ContractState;
use ark_core::contract::ContractType;
use ark_core::contract::DefaultVtxoContract;
use ark_core::contract::DelegateVtxoContract;
use ark_core::contract::StoredContract;
use ark_core::contract::VhtlcContract;
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
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::All;
use bitcoin::secp256k1::Message;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
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
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;

pub mod contract;
pub mod error;
pub mod key_provider;
pub mod swap_storage;
pub mod vtxo_watcher;
pub mod wallet;

mod asset;
mod batch;
mod boltz;
mod coin_select;
mod fee_estimation;
mod migration;
mod send_vtxo;
mod unilateral_exit;
mod utils;

pub use ark_core::server::DeprecatedSignerStatus;
pub use ark_core::server::ServerSignerStatus;
pub use asset::IssueAssetResult;
pub use boltz::ChainSwapAmount;
pub use boltz::ChainSwapData;
pub use boltz::ChainSwapDirection;
pub use boltz::ChainSwapResult;
pub use boltz::PendingVhtlcSpendTx;
pub use boltz::PendingVhtlcSpendType;
pub use boltz::ReverseSwapData;
pub use boltz::SubmarineSwapData;
pub use boltz::SwapAmount;
pub use boltz::SwapStatus;
pub use boltz::SwapStatusInfo;
pub use boltz::SwapType;
pub use boltz::TimeoutBlockHeights;
pub use contract::AnnotatedBoardingOutput;
pub use contract::AnnotatedVtxo;
pub use contract::AnnotatedVtxoList;
pub use contract::ContractManager;
pub use contract::ContractRegistry;
pub use contract::ContractStore;
pub use contract::MemoryContractStore;
#[cfg(feature = "sqlite")]
pub use contract::SqliteContractStore;
pub use error::Error;
pub use key_provider::Bip32KeyProvider;
pub use key_provider::DiscoverableKeyProvider;
pub use key_provider::KeyProvider;
pub use key_provider::StaticKeyProvider;
pub use lightning_invoice;
pub use migration::DeprecatedSignerMigrationReport;
pub use migration::DeprecatedSignerReport;
pub use migration::MigrationLegReport;
pub use migration::MigrationSkipReason;
pub use migration::MigrationVtxoRef;
pub use migration::MAX_VTXOS_PER_SETTLEMENT;
pub use swap_storage::InMemorySwapStorage;
#[cfg(feature = "sqlite")]
pub use swap_storage::SqliteSwapStorage;
pub use swap_storage::SwapStorage;

/// Default gap limit for BIP44-style key discovery
///
/// This is the number of consecutive unused addresses to scan before
/// assuming all used addresses have been found.
pub const DEFAULT_GAP_LIMIT: u32 = 20;

/// Default Boltz `referralId` sent with swap creation requests when the caller does not
/// provide one. Identifies traffic originating from this SDK.
pub const DEFAULT_BOLTZ_REFERRAL_ID: &str = "arkade-rs-SDK";

/// Summary returned by [`Client::restore_contracts`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContractRestoreReport {
    /// Gap limit used for this scan.
    pub gap_limit: u32,
    /// First derived key index that was probed, if any.
    pub scanned_from: Option<u32>,
    /// One past the last derived key index that was probed, if any.
    pub scanned_to_exclusive: Option<u32>,
    /// Derived key indexes that were probed.
    pub scanned_keys: u32,
    /// Key indexes where at least one contract had activity.
    pub discovered_key_indexes: Vec<u32>,
    /// Highest discovered key index, if any.
    pub last_used_key_index: Option<u32>,
    /// Suggested next receive key index, if any.
    pub next_key_index: Option<u32>,
    /// Offchain default/delegate contracts with VTXO activity.
    pub offchain_contracts: u32,
    /// Boarding contracts with on-chain UTXO activity.
    pub boarding_contracts: u32,
    /// Contracts that were not already in the store.
    pub inserted_contracts: u32,
    /// Discovered contracts that were already present in the store.
    pub known_contracts: u32,
    /// Per-contract discovery details for caller UX.
    pub entries: Vec<ContractRestoreEntry>,
}

impl ContractRestoreReport {
    pub fn discovered_keys(&self) -> u32 {
        self.discovered_key_indexes.len() as u32
    }

    pub fn discovered_contracts(&self) -> u32 {
        self.entries.len() as u32
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractRestoreEntry {
    pub key_index: u32,
    pub contract_type: ContractType,
    pub script_pubkey: ScriptBuf,
    pub status: ContractRestoreEntryStatus,
    pub discovery: ContractRestoreDiscovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractRestoreEntryStatus {
    Inserted,
    Known,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractRestoreDiscovery {
    Offchain {
        vtxos: Vec<ContractRestoreVtxo>,
    },
    Boarding {
        outpoints: Vec<ContractRestoreOutpoint>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractRestoreVtxo {
    pub outpoint: OutPoint,
    pub amount: Amount,
    pub is_spent: bool,
    pub is_swept: bool,
    pub is_unrolled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractRestoreOutpoint {
    pub outpoint: OutPoint,
    pub amount: Amount,
    pub confirmation_blocktime: Option<u64>,
    pub confirmations: u64,
}

/// Wallet-facing view of a stored contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractInfo {
    /// The validated stored contract row.
    pub contract: StoredContract,
    /// Address derived from the contract, when the SDK knows how to derive one.
    pub address: Option<String>,
    /// Kind of address in [`Self::address`].
    pub address_kind: Option<ContractAddressKind>,
    /// Server signer encoded in this contract, when the SDK knows how to decode it.
    pub server_pk: Option<XOnlyPublicKey>,
    /// Rotation status of [`Self::server_pk`] against the current server info.
    pub signer_status: Option<ServerSignerStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractAddressKind {
    Ark,
    Bitcoin,
}

/// Default mainnet Arkade server URL.
pub const ARKADE_MAINNET_URL: &str = "https://arkade.computer";

/// Default mutinynet Arkade server URL.
pub const ARKADE_MUTINYNET_URL: &str = "https://mutinynet.arkade.sh";

/// Default mainnet Boltz API URL.
pub const BOLTZ_MAINNET_URL: &str = "https://api.boltz.exchange";

/// Default mutinynet Boltz API URL.
pub const BOLTZ_MUTINYNET_URL: &str = "https://api.boltz.mutinynet.arkade.sh";

/// Default timeout for network operations.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default maximum age for cached Ark server info.
pub const DEFAULT_SERVER_INFO_TTL: Duration = Duration::from_secs(15 * 60);

/// Boltz referral ID behavior for swap creation requests.
#[derive(Clone, Debug, Default)]
pub enum BoltzReferralId {
    /// Use [`DEFAULT_BOLTZ_REFERRAL_ID`].
    #[default]
    Default,
    /// Send no `referralId` field with Boltz swap creation requests.
    Disabled,
    /// Send a custom `referralId` field with Boltz swap creation requests.
    Custom(String),
}

/// Configuration for constructing an [`OfflineClient`].
///
/// The default configuration targets mainnet. Set [`Self::server_info_ttl`] to
/// [`Duration::ZERO`] to refresh server info on every access.
#[derive(Clone, Debug)]
pub struct OfflineClientConfig {
    pub ark_server_url: String,
    pub boltz_url: String,
    pub timeout: Duration,
    pub server_info_ttl: Duration,
    pub boltz_referral_id: BoltzReferralId,
    pub delegator_pk: Option<XOnlyPublicKey>,
    pub historical_delegator_pks: Vec<XOnlyPublicKey>,
}

impl Default for OfflineClientConfig {
    fn default() -> Self {
        Self {
            ark_server_url: ARKADE_MAINNET_URL.to_string(),
            boltz_url: BOLTZ_MAINNET_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            server_info_ttl: DEFAULT_SERVER_INFO_TTL,
            boltz_referral_id: BoltzReferralId::default(),
            delegator_pk: None,
            historical_delegator_pks: Vec::new(),
        }
    }
}

/// A client to interact with Ark Server
///
/// ## Example
///
/// ```rust
/// # use std::future::Future;
/// # use std::str::FromStr;
/// # use ark_client::{Blockchain, Client, Error, SpendStatus, TxStatus};
/// # use ark_client::OfflineClient;
/// # use ark_client::OfflineClientConfig;
/// # use bitcoin::key::Keypair;
/// # use bitcoin::secp256k1::SecretKey;
/// # use std::sync::Arc;
/// # use bitcoin::{Address, Amount, FeeRate, Psbt, Transaction, Txid};
/// # use ark_client::wallet::{Balance, OnchainWallet};
/// # use ark_client::InMemorySwapStorage;
/// # use ark_core::{UtxoCoinSelection, ExplorerUtxo};
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
/// // Initialize the client with a static keypair
/// async fn init_client_with_keypair() -> Result<Client<MyBlockchain, MyWallet, InMemorySwapStorage>, ark_client::Error> {
///     // Create a keypair for signing transactions
///     let secp = bitcoin::key::Secp256k1::new();
///     let secret_key = SecretKey::from_str("your_private_key_here").unwrap();
///     let keypair = Keypair::from_secret_key(&secp, &secret_key);
///
///     // Initialize blockchain and wallet implementations
///     let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
///     let wallet = Arc::new(MyWallet {});
///
///     let config = OfflineClientConfig {
///         ark_server_url: "https://ark-server.example.com".to_string(),
///         boltz_url: "http://boltz.example.com".to_string(),
///         ..Default::default()
///     };
///
///     let offline_client = OfflineClient::with_keypair(
///         config,
///         keypair,
///         blockchain,
///         wallet,
///         Arc::new(InMemorySwapStorage::default()),
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
/// async fn init_client_with_bip32() -> Result<Client<MyBlockchain, MyWallet, InMemorySwapStorage>, ark_client::Error> {
///     // Create a BIP32 master key and derivation path
///     let master_key = Xpriv::from_str("xprv...").unwrap();
///     let derivation_path = DerivationPath::from_str("m/84'/0'/0'/0/0").unwrap();
///
///     // Initialize blockchain and wallet implementations
///     let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
///     let wallet = Arc::new(MyWallet {});
///
///     let config = OfflineClientConfig {
///         ark_server_url: "https://ark-server.example.com".to_string(),
///         boltz_url: "http://boltz.example.com".to_string(),
///         ..Default::default()
///     };
///
///     let offline_client = OfflineClient::with_bip32(
///         config,
///         master_key,
///         Some(derivation_path),
///         blockchain,
///         wallet,
///         Arc::new(InMemorySwapStorage::default()),
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
    key_provider: Arc<dyn KeyProvider>,
    discoverable_key_provider: Option<Arc<dyn DiscoverableKeyProvider>>,
    blockchain: Arc<B>,
    secp: Secp256k1<All>,
    wallet: Arc<W>,
    swap_storage: Arc<S>,
    boltz_url: String,
    boltz_referral_id: Option<String>,
    timeout: Duration,
    server_info_ttl: Duration,
    contract_store: Arc<Mutex<Option<Box<dyn ContractStore>>>>,
    delegator_pk: Option<XOnlyPublicKey>,
    historical_delegator_pks: Vec<XOnlyPublicKey>,
}

/// A client to interact with Ark server
///
/// See [`OfflineClient`] docs for details.
pub struct Client<B, W, S> {
    inner: OfflineClient<B, W, S>,
    state: Arc<RwLock<ServerState>>,
    server_info_refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

struct ServerState {
    server_info: server::Info,
    fee_estimator: ark_fees::Estimator,
    server_info_refreshed_at: Instant,
    contract_manager: Mutex<ContractManager>,
}

#[derive(Clone, Debug)]
enum RestoreCandidate {
    DefaultVtxo(DefaultVtxoContract),
    DelegateVtxo(DelegateVtxoContract),
    Boarding(BoardingContract),
}

#[derive(Clone, Debug)]
enum RestoreDiscoveryTarget {
    Offchain(ArkAddress),
    Boarding(Address),
}

impl RestoreCandidate {
    fn discovery_target(
        &self,
        secp: &Secp256k1<All>,
        ctx: &ContractContext,
    ) -> Result<RestoreDiscoveryTarget, Error> {
        match self {
            Self::DefaultVtxo(contract) => Ok(RestoreDiscoveryTarget::Offchain(
                Vtxo::new_default(
                    secp,
                    contract.server,
                    contract.owner,
                    contract.exit_delay,
                    ctx.network(),
                )?
                .to_ark_address(),
            )),
            Self::DelegateVtxo(contract) => Ok(RestoreDiscoveryTarget::Offchain(
                Vtxo::new_with_delegator(
                    secp,
                    contract.server,
                    contract.owner,
                    contract.delegator,
                    contract.exit_delay,
                    ctx.network(),
                )?
                .to_ark_address(),
            )),
            Self::Boarding(contract) => Ok(RestoreDiscoveryTarget::Boarding(
                contract.boarding_output(ctx)?.address().clone(),
            )),
        }
    }

    fn script_pubkey(
        &self,
        secp: &Secp256k1<All>,
        ctx: &ContractContext,
    ) -> Result<ScriptBuf, Error> {
        match self {
            Self::DefaultVtxo(contract) => Ok(Vtxo::new_default(
                secp,
                contract.server,
                contract.owner,
                contract.exit_delay,
                ctx.network(),
            )?
            .script_pubkey()),
            Self::DelegateVtxo(contract) => Ok(Vtxo::new_with_delegator(
                secp,
                contract.server,
                contract.owner,
                contract.delegator,
                contract.exit_delay,
                ctx.network(),
            )?
            .script_pubkey()),
            Self::Boarding(contract) => Ok(contract.boarding_output(ctx)?.script_pubkey()),
        }
    }

    fn contract_type(&self) -> ContractType {
        match self {
            Self::DefaultVtxo(_) => ContractType::default_vtxo(),
            Self::DelegateVtxo(_) => ContractType::delegate_vtxo(),
            Self::Boarding(_) => ContractType::boarding(),
        }
    }
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

#[derive(Clone, Debug, Default)]
pub struct OffChainBalance {
    pre_confirmed: Amount,
    confirmed: Amount,
    recoverable: Amount,
    /// Funds under a deprecated server signer whose cooperative-sign cutoff has passed.
    /// These VTXOs cannot be spent offchain (operator won't co-sign the old key) and are not yet
    /// recoverable (not expired). They will become recoverable once their VTXO expiry passes.
    pending_recovery: Amount,
    asset_balances: HashMap<AssetId, u64>,
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

    /// Funds locked under a deprecated signer past its cutoff — cannot be spent offchain,
    /// waiting for VTXO expiry to become recoverable. Still counted in `total()`.
    pub fn pending_recovery(&self) -> Amount {
        self.pending_recovery
    }

    pub fn total(&self) -> Amount {
        self.pre_confirmed + self.confirmed + self.recoverable + self.pending_recovery
    }

    /// Asset balances keyed by asset ID.
    pub fn asset_balances(&self) -> &HashMap<AssetId, u64> {
        &self.asset_balances
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

impl<B, W, S> OfflineClient<B, W, S>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    /// Create a new offline client with a generic key provider.
    pub fn with_key_provider(
        config: OfflineClientConfig,
        key_provider: Arc<dyn KeyProvider>,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        swap_storage: Arc<S>,
    ) -> Self {
        Self::with_key_provider_parts(config, key_provider, None, blockchain, wallet, swap_storage)
    }

    /// Create a new offline client with a discoverable key provider.
    pub fn with_discoverable_key_provider<P>(
        config: OfflineClientConfig,
        key_provider: Arc<P>,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        swap_storage: Arc<S>,
    ) -> Self
    where
        P: DiscoverableKeyProvider + 'static,
    {
        let core_key_provider: Arc<dyn KeyProvider> = key_provider.clone();
        let discoverable_key_provider: Arc<dyn DiscoverableKeyProvider> = key_provider;
        Self::with_key_provider_parts(
            config,
            core_key_provider,
            Some(discoverable_key_provider),
            blockchain,
            wallet,
            swap_storage,
        )
    }

    fn with_key_provider_parts(
        config: OfflineClientConfig,
        key_provider: Arc<dyn KeyProvider>,
        discoverable_key_provider: Option<Arc<dyn DiscoverableKeyProvider>>,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        swap_storage: Arc<S>,
    ) -> Self {
        let secp = Secp256k1::new();
        let network_client = ark_grpc::Client::new(config.ark_server_url);

        // Normalize historical delegator keys once (preserve order, remove duplicates), then
        // ensure the current delegator key is present at the front.
        let mut seen = HashSet::new();
        let mut historical_delegator_pks: Vec<_> = config
            .historical_delegator_pks
            .into_iter()
            .filter(|pk| seen.insert(*pk))
            .collect();

        if let Some(pk) = config.delegator_pk {
            historical_delegator_pks.retain(|k| *k != pk);
            historical_delegator_pks.insert(0, pk);
        }

        let boltz_referral_id = match config.boltz_referral_id {
            BoltzReferralId::Default => Some(DEFAULT_BOLTZ_REFERRAL_ID.to_string()),
            BoltzReferralId::Disabled => None,
            BoltzReferralId::Custom(referral_id) => Some(referral_id),
        };
        Self {
            network_client,
            key_provider,
            discoverable_key_provider,
            blockchain,
            secp,
            wallet,
            swap_storage,
            boltz_url: config.boltz_url.trim_end_matches('/').to_string(),
            boltz_referral_id,
            timeout: config.timeout,
            server_info_ttl: config.server_info_ttl,
            contract_store: Arc::new(Mutex::new(None)),
            delegator_pk: config.delegator_pk,
            historical_delegator_pks,
        }
    }

    /// Create a new offline client with a static keypair.
    pub fn with_keypair(
        config: OfflineClientConfig,
        kp: Keypair,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        swap_storage: Arc<S>,
    ) -> Self {
        let key_provider = Arc::new(StaticKeyProvider::new(kp));
        Self::with_key_provider(config, key_provider, blockchain, wallet, swap_storage)
    }

    /// Create a new offline client with an [`Xpriv`].
    pub fn with_bip32(
        config: OfflineClientConfig,
        xpriv: Xpriv,
        path: Option<DerivationPath>,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        swap_storage: Arc<S>,
    ) -> Self {
        let path = path.unwrap_or(
            DerivationPath::from_str(DEFAULT_DERIVATION_PATH).expect("valid derivation path"),
        );
        let key_provider = Arc::new(Bip32KeyProvider::new(xpriv, path));
        Self::with_discoverable_key_provider(config, key_provider, blockchain, wallet, swap_storage)
    }

    /// Use a custom contract store for the connected client.
    ///
    /// If unset, the client uses an in-memory contract store.
    pub fn with_contract_store(self, store: Box<dyn ContractStore>) -> Self {
        let mut contract_store = self
            .contract_store
            .lock()
            .expect("contract store lock should not be poisoned");
        *contract_store = Some(store);
        drop(contract_store);
        self
    }

    /// Returns the currently configured delegator pubkey, if any.
    pub fn delegator_pk(&self) -> Option<XOnlyPublicKey> {
        self.delegator_pk
    }

    /// Returns the Boltz referral ID sent with all swap creation requests, if any.
    pub fn boltz_referral_id(&self) -> Option<&str> {
        self.boltz_referral_id.as_deref()
    }

    fn contract_manager(&self, network: Network) -> Result<ContractManager, Error> {
        let store = self
            .contract_store
            .lock()
            .map_err(|_| Error::ad_hoc("contract store lock poisoned"))?
            .take()
            .unwrap_or_else(|| Box::new(MemoryContractStore::new()));
        Ok(ContractManager::new(network, store))
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

        self.finish_connect().await
    }

    async fn finish_connect(mut self) -> Result<Client<B, W, S>, Error> {
        let server_info = timeout_op(self.timeout, self.network_client.get_info())
            .await
            .context("Failed to get Ark server info")??;

        tracing::debug!(ark_server_url = ?self.network_client, "Connected to Ark server");

        let fee_estimator = build_fee_estimator(&server_info)?;
        let mut contract_manager = self.contract_manager(server_info.network)?;
        contract_manager.register_builtins()?;
        let state = Arc::new(RwLock::new(ServerState {
            server_info: server_info.clone(),
            fee_estimator,
            server_info_refreshed_at: Instant::now(),
            contract_manager: Mutex::new(contract_manager),
        }));
        let hook_state = state.clone();
        self.network_client
            .set_info_refresh_hook(move |server_info| {
                update_server_state(&hook_state, server_info)
                    .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
            });

        let client = Client {
            inner: self,
            state,
            server_info_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        client.hydrate_persisted_contract_keys()?;

        // Eagerly persist the bounded baseline contract set. This mirrors the TS SDK split:
        // connect() registers the always-watched index-0/current-key surface, while full
        // gap-limit wallet regeneration is explicit via restore_contracts().
        if let Err(error) = client.persist_baseline_contracts(&server_info) {
            tracing::warn!(?error, "Failed to persist baseline contracts at connect");
        }

        match client.migrate_boltz_vhtlc_contracts(&server_info).await {
            Ok(migrated) if migrated > 0 => {
                tracing::info!(migrated, "Migrated Boltz VHTLC contracts at connect");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(?error, "Failed to migrate Boltz VHTLC contracts at connect");
            }
        }

        Ok(client)
    }
}

fn contract_info_from_stored(
    contract: StoredContract,
    server_info: &server::Info,
    now_unix_secs: i64,
) -> Result<ContractInfo, Error> {
    let ctx = ContractContext::new(server_info.network);
    let (address, address_kind, server_pk) = match &contract.contract_type {
        contract_type if *contract_type == ContractType::default_vtxo() => {
            let data: DefaultVtxoContract =
                serde_json::from_value(contract.data.clone()).map_err(|e| {
                    Error::ad_hoc(format!("failed to decode default vtxo contract: {e}"))
                })?;
            (
                Some(data.vtxo(&ctx)?.to_ark_address().to_string()),
                Some(ContractAddressKind::Ark),
                Some(data.server),
            )
        }
        contract_type if *contract_type == ContractType::delegate_vtxo() => {
            let data: DelegateVtxoContract = serde_json::from_value(contract.data.clone())
                .map_err(|e| {
                    Error::ad_hoc(format!("failed to decode delegate vtxo contract: {e}"))
                })?;
            (
                Some(data.vtxo(&ctx)?.to_ark_address().to_string()),
                Some(ContractAddressKind::Ark),
                Some(data.server),
            )
        }
        contract_type if *contract_type == ContractType::boarding() => {
            let data: BoardingContract = serde_json::from_value(contract.data.clone())
                .map_err(|e| Error::ad_hoc(format!("failed to decode boarding contract: {e}")))?;
            (
                Some(data.boarding_output(&ctx)?.address().to_string()),
                Some(ContractAddressKind::Bitcoin),
                Some(data.server),
            )
        }
        contract_type if *contract_type == ContractType::vhtlc() => {
            let data: VhtlcContract = serde_json::from_value(contract.data.clone())
                .map_err(|e| Error::ad_hoc(format!("failed to decode vhtlc contract: {e}")))?;
            let address =
                ark_core::vhtlc::VhtlcScript::new(data.options.clone(), server_info.network)
                    .map_err(|e| Error::ad_hoc(format!("failed to build vhtlc address: {e}")))?
                    .address()
                    .to_string();
            (
                Some(address),
                Some(ContractAddressKind::Ark),
                Some(data.options.server),
            )
        }
        _ => {
            let address = Address::from_script(&contract.script_pubkey, server_info.network)
                .ok()
                .map(|address| address.to_string());
            let address_kind = address.as_ref().map(|_| ContractAddressKind::Bitcoin);
            (address, address_kind, None)
        }
    };
    let signer_status =
        server_pk.map(|server_pk| server_info.signer_status_at(server_pk, now_unix_secs));

    Ok(ContractInfo {
        contract,
        address,
        address_kind,
        server_pk,
        signer_status,
    })
}

fn build_fee_estimator(server_info: &server::Info) -> Result<ark_fees::Estimator, Error> {
    let fee_estimator_config = server_info
        .fees
        .clone()
        .map(|fees| ark_fees::Config {
            intent_offchain_input_program: fees.intent_fee.offchain_input.unwrap_or_default(),
            intent_onchain_input_program: fees.intent_fee.onchain_input.unwrap_or_default(),
            intent_offchain_output_program: fees.intent_fee.offchain_output.unwrap_or_default(),
            intent_onchain_output_program: fees.intent_fee.onchain_output.unwrap_or_default(),
        })
        .unwrap_or_default();

    ark_fees::Estimator::new(fee_estimator_config).map_err(Error::ark_server)
}

fn update_server_state(
    state: &Arc<RwLock<ServerState>>,
    server_info: server::Info,
) -> Result<(), Error> {
    let fee_estimator = build_fee_estimator(&server_info)?;
    let mut state = state
        .write()
        .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
    state.server_info = server_info;
    state.fee_estimator = fee_estimator;
    state.server_info_refreshed_at = Instant::now();
    Ok(())
}

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    /// Returns Ark server info, refreshing the cached snapshot when its TTL has expired.
    pub async fn server_info(&self) -> Result<server::Info, Error> {
        // Fast path avoids taking the async mutex while the cache is fresh.
        if let Some(server_info) = self.cached_server_info_if_fresh()? {
            return Ok(server_info);
        }

        let _guard = self.server_info_refresh_lock.lock().await;
        // Re-check after acquiring the mutex: another task may have refreshed while we waited.
        if let Some(server_info) = self.cached_server_info_if_fresh()? {
            return Ok(server_info);
        }

        self.refresh_server_info_unlocked().await
    }

    fn cached_server_info_if_fresh(&self) -> Result<Option<server::Info>, Error> {
        self.state
            .read()
            .map(|state| {
                (state.server_info_refreshed_at.elapsed() < self.inner.server_info_ttl)
                    .then(|| state.server_info.clone())
            })
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))
    }

    fn with_server_state<T>(&self, f: impl FnOnce(&ServerState) -> T) -> Result<T, Error> {
        self.state
            .read()
            .map(|state| f(&state))
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))
    }

    fn eval_onchain_output_fee(&self, output: ark_fees::Output) -> Result<Amount, Error> {
        self.with_server_state(|state| state.fee_estimator.eval_onchain_output(output))?
            .map(|fee| Amount::from_sat(fee.to_satoshis()))
            .map_err(Error::ad_hoc)
    }

    /// Force-fetch the latest Ark server `/info` and replace the cached server state.
    ///
    /// This bypasses the TTL used by [`Self::server_info`]. Concurrent refreshes are serialized
    /// with the same refresh gate used by TTL-based refreshes.
    ///
    /// Returns the freshly fetched server info. The refreshed snapshot includes the server's
    /// current [`server::Info::deprecated_signers`], allowing subsequent wallet operations to
    /// observe signer rotations.
    pub async fn refresh_server_info(&self) -> Result<server::Info, Error> {
        let _guard = self.server_info_refresh_lock.lock().await;
        self.refresh_server_info_unlocked().await
    }

    /// Fetch `/info` and update cached server state without acquiring the refresh gate.
    ///
    /// Callers must already hold `server_info_refresh_lock`, or otherwise guarantee that
    /// concurrent refreshes are serialized.
    async fn refresh_server_info_unlocked(&self) -> Result<server::Info, Error> {
        let server_info = timeout_op(self.inner.timeout, self.network_client().get_info())
            .await
            .context("Failed to refresh Ark server info")??;

        update_server_state(&self.state, server_info.clone())?;

        Ok(server_info)
    }

    /// Returns the currently configured delegator pubkey, if any.
    pub fn delegator_pk(&self) -> Option<XOnlyPublicKey> {
        self.inner.delegator_pk()
    }

    /// Returns the Boltz referral ID sent with all swap creation requests, if any.
    pub fn boltz_referral_id(&self) -> Option<&str> {
        self.inner.boltz_referral_id()
    }

    /// List all contracts currently known to this wallet.
    ///
    /// This is a wallet-facing view over the contract store: each row includes the stored contract,
    /// a derived address when the SDK knows the contract type, and signer-rotation status for
    /// contracts that encode a server signer.
    pub async fn list_contracts(&self) -> Result<Vec<ContractInfo>, Error> {
        let server_info = self.server_info().await?;
        let now = unix_now()?;
        let contracts = {
            let state = self
                .state
                .read()
                .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
            let contracts = state
                .contract_manager
                .lock()
                .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
                .list()?;
            contracts
        };

        contracts
            .into_iter()
            .map(|contract| contract_info_from_stored(contract, &server_info, now))
            .collect()
    }

    /// Get a new offchain receiving address.
    ///
    /// When a delegator is configured (via [`OfflineClientConfig::delegator_pk`]),
    /// returns a 3-leaf delegate address. Otherwise returns a standard 2-leaf address.
    ///
    /// For HD wallets, this will derive a new address each time it's called.
    /// For static key providers, this will always return the same address.
    pub async fn get_offchain_address(&self) -> Result<(ArkAddress, Vtxo), Error> {
        let server_info = self.server_info().await?;
        self.get_offchain_address_with_server_info(&server_info)
    }

    pub(crate) fn get_offchain_address_with_server_info(
        &self,
        server_info: &server::Info,
    ) -> Result<(ArkAddress, Vtxo), Error> {
        let server_signer = server_info.signer_pk.into();
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .public_key()
            .into();

        self.persist_offchain_vtxo_contract(server_info, server_signer, owner)
    }

    /// Get all known offchain addresses for this wallet.
    ///
    /// When a delegator is configured, this returns **both** the default (2-leaf) and delegate
    /// (3-leaf) addresses for each key, so that VTXOs at either address are visible. If
    /// historical delegator keys are set via `historical_delegator_pks` passed to
    /// [`OfflineClientConfig::historical_delegator_pks`], addresses for those are included too.
    pub async fn get_offchain_addresses(&self) -> Result<Vec<(ArkAddress, Vtxo)>, Error> {
        let server_info = self.server_info().await?;
        self.get_offchain_addresses_with_server_info(&server_info)
    }

    fn persist_baseline_contracts(&self, server_info: &server::Info) -> Result<(), Error> {
        self.persist_baseline_offchain_contracts(server_info)?;
        self.persist_watch_boarding_outputs(server_info)?;
        Ok(())
    }

    fn hydrate_persisted_contract_keys(&self) -> Result<(), Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let contracts = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .list()?;

        let mut indices: Vec<u32> = contracts
            .into_iter()
            .filter_map(|contract| contract.key_index)
            .collect();
        indices.sort_unstable();
        indices.dedup();

        let Some(key_provider) = self.inner.discoverable_key_provider.as_ref() else {
            return Ok(());
        };
        for index in indices {
            key_provider.cache_keypair_at_index(index)?;
        }

        Ok(())
    }

    fn persist_baseline_offchain_contracts(
        &self,
        server_info: &server::Info,
    ) -> Result<Vec<(ArkAddress, Vtxo)>, Error> {
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .x_only_public_key()
            .0;
        let candidate_delays = ark_core::candidate_exit_delays(
            server_info.unilateral_exit_delay,
            server_info.network,
        )?;

        let mut results = Vec::new();
        for server_signer in server_info.all_server_keys() {
            for exit_delay in &candidate_delays {
                results.push(self.persist_default_vtxo_contract(
                    server_info.network,
                    server_signer,
                    owner,
                    *exit_delay,
                )?);

                let mut seen = HashSet::new();
                for dpk in &self.inner.historical_delegator_pks {
                    if !seen.insert(dpk) {
                        continue;
                    }
                    results.push(self.persist_delegate_vtxo_contract(
                        server_info.network,
                        server_signer,
                        owner,
                        *dpk,
                        *exit_delay,
                    )?);
                }
            }
        }
        Ok(results)
    }

    pub(crate) fn get_offchain_addresses_with_server_info(
        &self,
        server_info: &server::Info,
    ) -> Result<Vec<(ArkAddress, Vtxo)>, Error> {
        let pks = self.inner.key_provider.get_cached_pks()?;

        // Build addresses for current signer + all deprecated signers so VTXOs under any
        // known server key are discovered and visible in the balance.
        let all_server_keys: Vec<XOnlyPublicKey> = server_info.all_server_keys().collect();
        // Enumerate under every candidate exit delay (the advertised delay plus, on mainnet, the
        // legacy delay), mirroring `restore_contracts`: a VTXO minted before the operator shortened
        // the delay lives at a legacy-delay address, so building only the current delay
        // would hide it from `list_vtxos`/balance/migration even though its key was
        // discovered. Off mainnet this is just the advertised delay (no behaviour change).
        let candidate_delays = ark_core::candidate_exit_delays(
            server_info.unilateral_exit_delay,
            server_info.network,
        )?;

        let mut results = Vec::new();

        for owner_pk in &pks {
            for server_signer in &all_server_keys {
                for exit_delay in &candidate_delays {
                    results.push(self.persist_default_vtxo_contract(
                        server_info.network,
                        *server_signer,
                        *owner_pk,
                        *exit_delay,
                    )?);

                    // Delegate addresses for all known delegator keys.
                    let mut seen = HashSet::new();
                    for dpk in &self.inner.historical_delegator_pks {
                        if !seen.insert(dpk) {
                            continue;
                        }
                        results.push(self.persist_delegate_vtxo_contract(
                            server_info.network,
                            *server_signer,
                            *owner_pk,
                            *dpk,
                            *exit_delay,
                        )?);
                    }
                }
            }
        }

        Ok(results)
    }

    fn persist_offchain_vtxo_contract(
        &self,
        server_info: &server::Info,
        server_signer: XOnlyPublicKey,
        owner: XOnlyPublicKey,
    ) -> Result<(ArkAddress, Vtxo), Error> {
        match self.inner.delegator_pk {
            Some(delegator) => self.persist_delegate_vtxo_contract(
                server_info.network,
                server_signer,
                owner,
                delegator,
                server_info.unilateral_exit_delay,
            ),
            None => self.persist_default_vtxo_contract(
                server_info.network,
                server_signer,
                owner,
                server_info.unilateral_exit_delay,
            ),
        }
    }

    fn persist_default_vtxo_contract(
        &self,
        network: Network,
        server: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
    ) -> Result<(ArkAddress, Vtxo), Error> {
        let key_index = self.derivation_index_for_pk(&owner);
        let contract = DefaultVtxoContract {
            server,
            owner,
            exit_delay,
        };
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        manager.insert_or_get(contract.clone(), ContractState::Active, key_index)?;
        let ctx = ContractContext::new(network);
        // Derive from the requested default VTXO contract, not from the stored row: the store may
        // already contain an equivalent boarding row for this script, but the caller still needs
        // the offchain Arkade address for this default VTXO script.
        let vtxo = contract.vtxo(&ctx)?;
        Ok((vtxo.to_ark_address(), vtxo))
    }

    fn persist_delegate_vtxo_contract(
        &self,
        network: Network,
        server: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        delegator: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
    ) -> Result<(ArkAddress, Vtxo), Error> {
        let key_index = self.derivation_index_for_pk(&owner);
        let contract = DelegateVtxoContract {
            server,
            owner,
            delegator,
            exit_delay,
        };
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        let stored = manager.insert_or_get(contract, ContractState::Active, key_index)?;
        let contract = manager
            .get_typed::<DelegateVtxoContract>(&stored.script_pubkey)?
            .ok_or_else(|| Error::ad_hoc("missing delegate vtxo contract"))?;
        let ctx = ContractContext::new(network);
        let vtxo = contract.vtxo(&ctx)?;
        Ok((vtxo.to_ark_address(), vtxo))
    }

    /// Restore persisted contracts using explicit contract discovery.
    ///
    /// This method derives candidate default VTXO, delegate VTXO, and boarding contracts for each
    /// key index. It queries the Arkade VTXO index for offchain candidates and the configured
    /// blockchain backend for boarding candidates, persists contracts that have activity, and stops
    /// when a full batch has no discovered contracts.
    ///
    /// No-op for StaticKeyProvider.
    ///
    /// # Arguments
    ///
    /// * `gap_limit` - Number of consecutive unused key indexes before stopping
    pub async fn restore_contracts(&self, gap_limit: u32) -> Result<ContractRestoreReport, Error> {
        if gap_limit == 0 {
            return Err(Error::ad_hoc("restore gap limit must be greater than zero"));
        }

        let Some(key_provider) = self.inner.discoverable_key_provider.as_ref() else {
            tracing::debug!("Key provider does not support discovery, skipping");
            return Ok(ContractRestoreReport {
                gap_limit,
                ..Default::default()
            });
        };

        let server_info = &self.server_info().await?;
        let ctx = ContractContext::new(server_info.network);
        let all_server_keys: Vec<XOnlyPublicKey> = server_info.all_server_keys().collect();
        let offchain_exit_delays = ark_core::candidate_exit_delays(
            server_info.unilateral_exit_delay,
            server_info.network,
        )?;
        let boarding_exit_delays =
            ark_core::candidate_exit_delays(server_info.boarding_exit_delay, server_info.network)?;

        let mut start_index = 0u32;
        let mut report = ContractRestoreReport {
            gap_limit,
            ..Default::default()
        };

        tracing::info!(gap_limit, "Starting contract restore");

        loop {
            let batch = self.restore_candidate_batch(
                start_index,
                gap_limit,
                &all_server_keys,
                &offchain_exit_delays,
                &boarding_exit_delays,
            )?;

            if batch.is_empty() {
                break;
            }

            report.scanned_from.get_or_insert(start_index);
            let scanned_to_exclusive = start_index
                .checked_add(batch.len() as u32)
                .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;
            report.scanned_to_exclusive = Some(scanned_to_exclusive);
            report.scanned_keys += batch.len() as u32;

            let mut offchain_addresses = Vec::new();
            for (_, _, candidates) in &batch {
                for candidate in candidates {
                    if let RestoreDiscoveryTarget::Offchain(address) =
                        candidate.discovery_target(self.secp(), &ctx)?
                    {
                        offchain_addresses.push(address);
                    }
                }
            }
            let vtxo_list = self
                .list_vtxos_for_addresses(offchain_addresses.into_iter())
                .await?;
            let mut offchain_vtxos_by_script =
                HashMap::<ScriptBuf, Vec<ContractRestoreVtxo>>::new();
            for vtxo in vtxo_list.all() {
                offchain_vtxos_by_script
                    .entry(vtxo.script.clone())
                    .or_default()
                    .push(ContractRestoreVtxo {
                        outpoint: vtxo.outpoint,
                        amount: vtxo.amount,
                        is_spent: vtxo.is_spent,
                        is_swept: vtxo.is_swept,
                        is_unrolled: vtxo.is_unrolled,
                    });
            }

            let mut found_any = false;
            for (index, kp, candidates) in batch {
                let mut found_for_key = false;

                for candidate in candidates {
                    let script = candidate.script_pubkey(self.secp(), &ctx)?;
                    let contract_type = candidate.contract_type();
                    let target = candidate.discovery_target(self.secp(), &ctx)?;
                    let discovery = match target {
                        RestoreDiscoveryTarget::Offchain(_) => offchain_vtxos_by_script
                            .get(&script)
                            .filter(|vtxos| !vtxos.is_empty())
                            .cloned()
                            .map(|vtxos| ContractRestoreDiscovery::Offchain { vtxos }),
                        RestoreDiscoveryTarget::Boarding(address) => {
                            let outpoints = self
                                .blockchain()
                                .find_outpoints(&address)
                                .await?
                                .into_iter()
                                .filter(|utxo| !utxo.is_spent)
                                .map(|utxo| ContractRestoreOutpoint {
                                    outpoint: utxo.outpoint,
                                    amount: utxo.amount,
                                    confirmation_blocktime: utxo.confirmation_blocktime,
                                    confirmations: utxo.confirmations,
                                })
                                .collect::<Vec<_>>();
                            (!outpoints.is_empty())
                                .then_some(ContractRestoreDiscovery::Boarding { outpoints })
                        }
                    };

                    let Some(discovery) = discovery else {
                        continue;
                    };

                    let inserted = self.persist_restore_candidate(candidate, index)?;
                    let status = if inserted {
                        report.inserted_contracts += 1;
                        ContractRestoreEntryStatus::Inserted
                    } else {
                        report.known_contracts += 1;
                        ContractRestoreEntryStatus::Known
                    };
                    match &discovery {
                        ContractRestoreDiscovery::Offchain { .. } => report.offchain_contracts += 1,
                        ContractRestoreDiscovery::Boarding { .. } => report.boarding_contracts += 1,
                    }
                    report.entries.push(ContractRestoreEntry {
                        key_index: index,
                        contract_type,
                        script_pubkey: script,
                        status,
                        discovery,
                    });
                    found_for_key = true;
                }

                if found_for_key {
                    key_provider.cache_discovered_keypair(index, kp)?;
                    report.discovered_key_indexes.push(index);
                    report.last_used_key_index = Some(
                        report
                            .last_used_key_index
                            .map_or(index, |last| last.max(index)),
                    );
                    report.next_key_index = report
                        .last_used_key_index
                        .and_then(|last| last.checked_add(1));
                    found_any = true;
                }
            }

            if !found_any {
                break;
            }

            start_index = start_index
                .checked_add(gap_limit)
                .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;
        }

        tracing::info!(?report, "Contract restore completed");

        Ok(report)
    }

    fn restore_candidate_batch(
        &self,
        start_index: u32,
        gap_limit: u32,
        server_keys: &[XOnlyPublicKey],
        offchain_exit_delays: &[bitcoin::Sequence],
        boarding_exit_delays: &[bitcoin::Sequence],
    ) -> Result<Vec<(u32, Keypair, Vec<RestoreCandidate>)>, Error> {
        let mut batch = Vec::with_capacity(gap_limit as usize);

        for i in 0..gap_limit {
            let index = start_index
                .checked_add(i)
                .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;
            let Some(key_provider) = self.inner.discoverable_key_provider.as_ref() else {
                break;
            };
            let Some(kp) = key_provider.derive_at_discovery_index(index)? else {
                break;
            };
            let owner = kp.x_only_public_key().0;
            let mut candidates = Vec::new();

            for server in server_keys {
                for exit_delay in offchain_exit_delays {
                    candidates.push(RestoreCandidate::DefaultVtxo(DefaultVtxoContract {
                        server: *server,
                        owner,
                        exit_delay: *exit_delay,
                    }));

                    let mut seen_delegators = HashSet::new();
                    for delegator in &self.inner.historical_delegator_pks {
                        if !seen_delegators.insert(delegator) {
                            continue;
                        }
                        candidates.push(RestoreCandidate::DelegateVtxo(DelegateVtxoContract {
                            server: *server,
                            owner,
                            delegator: *delegator,
                            exit_delay: *exit_delay,
                        }));
                    }
                }

                for exit_delay in boarding_exit_delays {
                    candidates.push(RestoreCandidate::Boarding(BoardingContract {
                        server: *server,
                        owner,
                        exit_delay: *exit_delay,
                    }));
                }
            }

            batch.push((index, kp, candidates));
        }

        Ok(batch)
    }

    fn persist_restore_candidate(
        &self,
        candidate: RestoreCandidate,
        key_index: u32,
    ) -> Result<bool, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        let ctx = ContractContext::new(state.server_info.network);
        let script = candidate.script_pubkey(self.secp(), &ctx)?;
        let existed = manager.get(&script)?.is_some();

        match candidate {
            RestoreCandidate::DefaultVtxo(contract) => {
                manager.insert_or_get(contract, ContractState::Active, Some(key_index))?;
            }
            RestoreCandidate::DelegateVtxo(contract) => {
                manager.insert_or_get(contract, ContractState::Active, Some(key_index))?;
            }
            RestoreCandidate::Boarding(contract) => {
                manager.insert_or_get(contract, ContractState::Active, Some(key_index))?;
            }
        }

        Ok(!existed)
    }

    // At the moment we are always generating the same address.
    pub async fn get_boarding_address(&self) -> Result<Address, Error> {
        let server_info = &self.server_info().await?;
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .x_only_public_key()
            .0;

        let contract = BoardingContract {
            server: server_info.signer_pk.into(),
            owner,
            exit_delay: server_info.boarding_exit_delay,
        };
        let key_index = self.derivation_index_for_pk(&owner);
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let stored = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .insert_or_get(contract, ContractState::Active, key_index)?;

        Address::from_script(&stored.script_pubkey, server_info.network)
            .map_err(|e| Error::ad_hoc(format!("invalid boarding contract script: {e}")))
    }

    pub fn get_onchain_address(&self) -> Result<Address, Error> {
        self.inner.wallet.get_onchain_address()
    }

    pub async fn get_boarding_addresses(&self) -> Result<Vec<Address>, Error> {
        let server_info = &self.server_info().await?;

        // Persist a boarding output for every (signer, exit-delay) candidate and return the
        // de-duplicated addresses. This is the watch/history surface: it covers the current signer
        // plus all deprecated signers (server-key rotation), each crossed with the candidate
        // exit-delay set (the advertised delay plus, on mainnet, the legacy delay) so deposits
        // minted under an older delay or an older key are still visible.
        //
        // The spend path (settle) deliberately stays current-signer-only;
        // deprecated-signer boarding recovery is handled via migrate_deprecated_signer_vtxos().
        let outputs = self.persist_watch_boarding_outputs(server_info)?;

        let mut seen = HashSet::new();
        let mut addresses = Vec::with_capacity(outputs.len());
        for output in &outputs {
            let address = output.address().clone();
            if seen.insert(address.clone()) {
                addresses.push(address);
            }
        }

        Ok(addresses)
    }

    /// Persist (idempotently) a boarding output for each signer the wallet should watch crossed
    /// with each candidate exit delay, returning annotated boarding outputs.
    ///
    /// Covers the current signer plus every deprecated signer, each paired with
    /// [`ark_core::candidate_exit_delays`] (the advertised boarding-exit delay plus, on mainnet,
    /// the legacy delay). Re-persisting the same boarding contract is idempotent, so this is safe
    /// to call repeatedly — at connect time and again from [`Client::get_boarding_addresses`].
    fn persist_watch_boarding_outputs(
        &self,
        server_info: &server::Info,
    ) -> Result<Vec<AnnotatedBoardingOutput>, Error> {
        let candidate_delays =
            ark_core::candidate_exit_delays(server_info.boarding_exit_delay, server_info.network)?;
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .x_only_public_key()
            .0;
        let key_index = self.derivation_index_for_pk(&owner);
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;

        for server_pk in server_info.all_server_keys() {
            for exit_delay in &candidate_delays {
                let contract = BoardingContract {
                    server: server_pk,
                    owner,
                    exit_delay: *exit_delay,
                };
                manager.insert_or_get(contract, ContractState::Active, key_index)?;
            }
        }

        manager.annotated_boarding_outputs_for_exit_delays(&candidate_delays)
    }

    pub async fn get_virtual_tx_outpoints(
        &self,
        addresses: impl Iterator<Item = ArkAddress>,
    ) -> Result<Vec<VirtualTxOutPoint>, Error> {
        let request = GetVtxosRequest::new_for_addresses(addresses);
        self.fetch_all_vtxos(request).await
    }

    pub async fn list_vtxos(&self) -> Result<AnnotatedVtxoList, Error> {
        let server_info = self.server_info().await?;
        self.list_vtxos_with_server_info(&server_info).await
    }

    pub(crate) async fn list_vtxos_with_server_info(
        &self,
        server_info: &server::Info,
    ) -> Result<AnnotatedVtxoList, Error> {
        let addresses = self.active_offchain_contract_addresses()?;
        let virtual_tx_outpoints = self.get_virtual_tx_outpoints(addresses.into_iter()).await?;
        let contract_vtxos = self.annotate_vtxos(virtual_tx_outpoints)?;

        Ok(AnnotatedVtxoList::new(server_info.dust, contract_vtxos))
    }

    pub async fn list_vtxos_for_addresses(
        &self,
        addresses: impl Iterator<Item = ArkAddress>,
    ) -> Result<VtxoList, Error> {
        let server_info = self.server_info().await?;
        self.list_vtxos_for_addresses_with_server_info(&server_info, addresses)
            .await
    }

    pub(crate) async fn list_vtxos_for_addresses_with_server_info(
        &self,
        server_info: &server::Info,
        addresses: impl Iterator<Item = ArkAddress>,
    ) -> Result<VtxoList, Error> {
        let virtual_tx_outpoints = self
            .get_virtual_tx_outpoints(addresses)
            .await
            .context("failed to get VTXOs for addresses")?;

        let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

        Ok(vtxo_list)
    }

    pub async fn list_vtxos_for_outpoints(
        &self,
        outpoints: Vec<OutPoint>,
    ) -> Result<AnnotatedVtxoList, Error> {
        let request = GetVtxosRequest::new_for_outpoints(&outpoints);
        let virtual_tx_outpoints = self.fetch_all_vtxos(request).await?;
        let contract_vtxos = self.annotate_vtxos(virtual_tx_outpoints)?;
        Ok(AnnotatedVtxoList::new(
            self.server_info().await?.dust,
            contract_vtxos,
        ))
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
        let vtxo_list = self.list_vtxos().await.context("failed to list VTXOs")?;
        let now = unix_now()?;
        let server_info = self.server_info().await?;

        let spendable_outpoints: HashSet<OutPoint> = vtxo_list
            .spendable_offchain_at(&server_info, now)
            .map(|entry| entry.vtxo().outpoint)
            .collect();

        let pre_confirmed = vtxo_list
            .pre_confirmed()
            .filter(|entry| spendable_outpoints.contains(&entry.vtxo().outpoint))
            .fold(Amount::ZERO, |acc, entry| acc + entry.vtxo().amount);

        let confirmed = vtxo_list
            .confirmed()
            .filter(|entry| spendable_outpoints.contains(&entry.vtxo().outpoint))
            .fold(Amount::ZERO, |acc, entry| acc + entry.vtxo().amount);

        let recoverable = vtxo_list
            .recoverable()
            .fold(Amount::ZERO, |acc, entry| acc + entry.vtxo().amount);

        let pending_recovery = vtxo_list
            .pending_recovery_due_to_signer_at(&server_info, now)
            .fold(Amount::ZERO, |acc, entry| acc + entry.vtxo().amount);

        // Aggregate asset balances from currently offchain-spendable VTXOs only.
        let mut asset_balances: HashMap<AssetId, u64> = HashMap::new();
        for entry in vtxo_list.spendable_offchain_at(&server_info, now) {
            for asset in &entry.vtxo().assets {
                let total = asset_balances
                    .get(&asset.asset_id)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(asset.amount)
                    .ok_or_else(|| Error::ad_hoc("asset balance overflow"))?;
                asset_balances.insert(asset.asset_id, total);
            }
        }

        Ok(OffChainBalance {
            pre_confirmed,
            confirmed,
            recoverable,
            pending_recovery,
            asset_balances,
        })
    }

    /// Get information about an asset by its ID.
    pub async fn get_asset(&self, asset_id: AssetId) -> Result<server::AssetInfo, Error> {
        timeout_op(
            self.inner.timeout,
            self.network_client().get_asset(asset_id),
        )
        .await
        .context("Failed to get asset info")?
        .map_err(Error::ark_server)
    }

    pub async fn transaction_history(&self) -> Result<Vec<history::Transaction>, Error> {
        let mut boarding_transactions = Vec::new();
        let mut boarding_commitment_transactions = Vec::new();

        let boarding_addresses = self.get_boarding_addresses().await?;
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

        let vtxo_list = self.list_vtxos().await?;

        let spent_outpoints = vtxo_list
            .spent()
            .map(|entry| entry.vtxo().clone())
            .collect::<Vec<_>>();
        let unspent_outpoints = vtxo_list
            .all_unspent()
            .map(|entry| entry.vtxo().clone())
            .collect::<Vec<_>>();

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

    /// The server's dust threshold amount.
    pub async fn dust(&self) -> Result<Amount, Error> {
        Ok(self.server_info().await?.dust)
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

    fn sign_for_pk(&self, pk: &XOnlyPublicKey, msg: &Message) -> Result<Signature, Error> {
        let keypair = self.keypair_by_pk(pk)?;
        Ok(self.secp().sign_schnorr_no_aux_rand(msg, &keypair))
    }

    fn boarding_outputs(&self) -> Result<Vec<AnnotatedBoardingOutput>, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        // Include default VTXO rows only when their CSV delay matches a boarding delay candidate.
        // This covers the equal-delay case where a default VTXO row is the stored row for a script
        // that can also be used for boarding, without turning every default VTXO receive script
        // into a boarding watch.
        let candidate_delays = ark_core::candidate_exit_delays(
            state.server_info.boarding_exit_delay,
            state.server_info.network,
        )?;
        let outputs = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .annotated_boarding_outputs_for_exit_delays(&candidate_delays)?;
        Ok(outputs)
    }

    fn active_offchain_contract_addresses(&self) -> Result<Vec<ArkAddress>, Error> {
        self.active_offchain_contracts().map(|contracts| {
            contracts
                .into_iter()
                .map(|contract| contract.address)
                .collect()
        })
    }

    fn active_offchain_contracts(&self) -> Result<Vec<contract::ActiveOffchainContract>, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let candidate_delays = ark_core::candidate_exit_delays(
            state.server_info.unilateral_exit_delay,
            state.server_info.network,
        )?;
        let contracts = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .active_offchain_contracts(&candidate_delays)?;
        Ok(contracts)
    }

    fn annotate_vtxos(&self, vtxos: Vec<VirtualTxOutPoint>) -> Result<Vec<AnnotatedVtxo>, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let annotated = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .annotate_vtxos(vtxos)?;
        Ok(annotated)
    }

    fn derivation_index_for_pk(&self, pk: &XOnlyPublicKey) -> Option<u32> {
        self.inner
            .discoverable_key_provider
            .as_ref()
            .and_then(|provider| provider.get_derivation_index_for_pk(pk))
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

#[cfg(test)]
mod digest_guard_tests {
    use super::*;
    use ark_grpc::test_utils;
    use bitcoin::key::Secp256k1;
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::Address;
    use bitcoin::FeeRate;
    use bitcoin::Network;
    use bitcoin::Psbt;
    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use tokio::net::TcpListener;
    use tonic::body::Body;
    use tonic::codegen::http;
    use tonic::codegen::Service;
    use tonic::server::NamedService;
    use tonic::server::UnaryService;

    #[derive(Clone, Default)]
    struct MockArkServer {
        state: Arc<MockState>,
    }

    #[derive(Default)]
    struct MockState {
        get_info_calls: AtomicUsize,
        list_vtxos_calls: AtomicUsize,
    }

    #[derive(Clone)]
    struct DummyBlockchain;

    impl Blockchain for DummyBlockchain {
        async fn find_outpoints(&self, _address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
            Ok(Vec::new())
        }

        async fn find_tx(&self, _txid: &Txid) -> Result<Option<Transaction>, Error> {
            Ok(None)
        }

        async fn get_tx_status(&self, _txid: &Txid) -> Result<TxStatus, Error> {
            Ok(TxStatus { confirmed_at: None })
        }

        async fn get_output_status(&self, _txid: &Txid, _vout: u32) -> Result<SpendStatus, Error> {
            Ok(SpendStatus { spend_txid: None })
        }

        async fn broadcast(&self, _tx: &Transaction) -> Result<(), Error> {
            Ok(())
        }

        async fn get_fee_rate(&self) -> Result<f64, Error> {
            Ok(1.0)
        }

        async fn broadcast_package(&self, _txs: &[&Transaction]) -> Result<(), Error> {
            Ok(())
        }
    }

    struct DummyWallet {
        keypair: Keypair,
        secp: Secp256k1<All>,
    }

    impl DummyWallet {
        fn new() -> Self {
            let secp = Secp256k1::new();
            let secret_key = SecretKey::from_slice(&[2; 32]).unwrap();
            let keypair = Keypair::from_secret_key(&secp, &secret_key);
            Self { keypair, secp }
        }
    }

    impl OnchainWallet for DummyWallet {
        fn get_onchain_address(&self) -> Result<Address, Error> {
            Ok(Address::p2tr(
                &self.secp,
                self.keypair.x_only_public_key().0,
                None,
                Network::Regtest,
            ))
        }

        async fn sync(&self) -> Result<(), Error> {
            Ok(())
        }

        fn balance(&self) -> Result<wallet::Balance, Error> {
            Ok(wallet::Balance {
                immature: Amount::ZERO,
                trusted_pending: Amount::ZERO,
                untrusted_pending: Amount::ZERO,
                confirmed: Amount::ZERO,
            })
        }

        fn prepare_send_to_address(
            &self,
            _address: Address,
            _amount: Amount,
            _fee_rate: FeeRate,
        ) -> Result<Psbt, Error> {
            Err(Error::wallet("not implemented"))
        }

        fn sign(&self, _psbt: &mut Psbt) -> Result<bool, Error> {
            Ok(true)
        }

        fn select_coins(&self, _target_amount: Amount) -> Result<UtxoCoinSelection, Error> {
            Err(Error::wallet("not implemented"))
        }
    }

    impl Service<http::Request<Body>> for MockArkServer {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            match req.uri().path() {
                "/ark.v1.ArkService/GetInfo" => {
                    let method = GetInfoSvc {
                        state: self.state.clone(),
                    };
                    Box::pin(async move {
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec);
                        Ok(grpc.unary(method, req).await)
                    })
                }
                "/ark.v1.IndexerService/GetVtxos" => {
                    let method = ListVtxosSvc {
                        state: self.state.clone(),
                    };
                    Box::pin(async move {
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec);
                        Ok(grpc.unary(method, req).await)
                    })
                }
                _ => Box::pin(async move {
                    Ok(http::Response::builder()
                        .status(200)
                        .header("grpc-status", "12")
                        .header("content-type", "application/grpc")
                        .body(Body::empty())
                        .unwrap())
                }),
            }
        }
    }

    impl NamedService for MockArkServer {
        const NAME: &'static str = "ark.v1.ArkService";
    }

    #[derive(Clone)]
    struct MockIndexerServer(MockArkServer);

    impl Service<http::Request<Body>> for MockIndexerServer {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = <MockArkServer as Service<http::Request<Body>>>::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            self.0.call(req)
        }
    }

    impl NamedService for MockIndexerServer {
        const NAME: &'static str = "ark.v1.IndexerService";
    }

    #[derive(Clone)]
    struct GetInfoSvc {
        state: Arc<MockState>,
    }

    impl UnaryService<test_utils::GetInfoRequest> for GetInfoSvc {
        type Response = test_utils::GetInfoResponse;
        type Future = Pin<
            Box<dyn Future<Output = Result<tonic::Response<Self::Response>, tonic::Status>> + Send>,
        >;

        fn call(&mut self, _request: tonic::Request<test_utils::GetInfoRequest>) -> Self::Future {
            self.state.get_info_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(tonic::Response::new(info_response("fresh-digest"))) })
        }
    }

    #[derive(Clone)]
    struct ListVtxosSvc {
        state: Arc<MockState>,
    }

    impl UnaryService<test_utils::GetVtxosRequest> for ListVtxosSvc {
        type Response = test_utils::GetVtxosResponse;
        type Future = Pin<
            Box<dyn Future<Output = Result<tonic::Response<Self::Response>, tonic::Status>> + Send>,
        >;

        fn call(&mut self, _request: tonic::Request<test_utils::GetVtxosRequest>) -> Self::Future {
            self.state.list_vtxos_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(tonic::Status::failed_precondition(
                    "DIGEST_MISMATCH: invalid digest header",
                ))
            })
        }
    }

    async fn connect_test_client(
        mock: MockArkServer,
    ) -> Client<DummyBlockchain, DummyWallet, InMemorySwapStorage> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let indexer_mock = MockIndexerServer(mock.clone());
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(mock)
                .add_service(indexer_mock)
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
        OfflineClient::<DummyBlockchain, DummyWallet, InMemorySwapStorage>::with_keypair(
            OfflineClientConfig {
                ark_server_url: format!("http://{addr}"),
                boltz_url: "http://127.0.0.1:1".to_string(),
                ..Default::default()
            },
            keypair,
            Arc::new(DummyBlockchain),
            Arc::new(DummyWallet::new()),
            Arc::new(InMemorySwapStorage::default()),
        )
        .connect()
        .await
        .unwrap()
    }

    fn info_response(digest: &str) -> test_utils::GetInfoResponse {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
        let keypair = Keypair::from_secret_key(&secp, &secret_key);
        let public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let (xonly, _) = keypair.x_only_public_key();
        let address = Address::p2tr(&secp, xonly, None, Network::Regtest);

        test_utils::GetInfoResponse {
            version: "0.9.9".to_string(),
            signer_pubkey: public_key.to_string(),
            forfeit_pubkey: public_key.to_string(),
            forfeit_address: address.to_string(),
            checkpoint_tapscript: String::new(),
            network: "regtest".to_string(),
            session_duration: 60,
            unilateral_exit_delay: 144,
            boarding_exit_delay: 144,
            utxo_min_amount: 0,
            utxo_max_amount: 0,
            vtxo_min_amount: 0,
            vtxo_max_amount: 0,
            dust: 1000,
            fees: None,
            scheduled_session: None,
            deprecated_signers: Vec::new(),
            service_status: Default::default(),
            digest: digest.to_string(),
            max_tx_weight: 0,
            max_op_return_outputs: 0,
        }
    }

    #[tokio::test]
    async fn server_info_uses_fresh_cache_without_refetching() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mock = MockArkServer::default();
        let state = mock.state.clone();
        let client = connect_test_client(mock).await;
        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 1);

        let info = client.server_info().await.unwrap();
        assert_eq!(info.digest, "fresh-digest");
        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_contracts_returns_wallet_contract_views() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = connect_test_client(MockArkServer::default()).await;
        let contracts = client.list_contracts().await.unwrap();

        assert!(!contracts.is_empty());
        assert!(contracts.iter().all(|entry| entry.address.is_some()));
        assert!(contracts.iter().all(|entry| entry.signer_status.is_some()));
    }

    #[tokio::test]
    async fn boarding_addresses_include_default_row_when_scripts_overlap() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = connect_test_client(MockArkServer::default()).await;
        let addresses = client.get_boarding_addresses().await.unwrap();

        assert_eq!(addresses.len(), 1);
    }

    #[tokio::test]
    async fn restore_contracts_rejects_zero_gap_limit() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = connect_test_client(MockArkServer::default()).await;
        let err = client.restore_contracts(0).await.unwrap_err();

        assert!(err.to_string().contains("gap limit"));
    }

    #[tokio::test]
    async fn restore_contracts_reports_gap_limit_for_static_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = connect_test_client(MockArkServer::default()).await;
        let report = client.restore_contracts(20).await.unwrap();

        assert_eq!(report.gap_limit, 20);
        assert_eq!(report.scanned_keys, 0);
        assert!(report.entries.is_empty());
    }

    #[tokio::test]
    async fn server_info_zero_ttl_always_refreshes() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mock = MockArkServer::default();
        let state = mock.state.clone();
        let mut client = connect_test_client(mock).await;
        client.inner.server_info_ttl = Duration::ZERO;
        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 1);

        client.server_info().await.unwrap();
        client.server_info().await.unwrap();

        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn server_info_refreshes_expired_cache_once_for_concurrent_callers() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mock = MockArkServer::default();
        let state = mock.state.clone();
        let client = Arc::new(connect_test_client(mock).await);
        client.state.write().unwrap().server_info_refreshed_at =
            Instant::now() - DEFAULT_SERVER_INFO_TTL - Duration::from_secs(1);

        let a = Arc::clone(&client);
        let b = Arc::clone(&client);
        let (info_a, info_b) = tokio::join!(a.server_info(), b.server_info());
        assert_eq!(info_a.unwrap().digest, "fresh-digest");
        assert_eq!(info_b.unwrap().digest, "fresh-digest");
        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn guarded_client_refreshes_info_and_does_not_retry_on_digest_mismatch() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mock = MockArkServer::default();
        let state = mock.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let indexer_mock = MockIndexerServer(mock.clone());
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(mock)
                .add_service(indexer_mock)
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        let mut inner = ark_grpc::Client::new(format!("http://{addr}"));
        inner.connect().await.unwrap();

        let initial_info: server::Info = info_response("stale-digest").try_into().unwrap();
        let mut contract_manager = ContractManager::in_memory(initial_info.network);
        contract_manager.register_builtins().unwrap();
        let cached_state = Arc::new(RwLock::new(ServerState {
            server_info: initial_info,
            fee_estimator: build_fee_estimator(&info_response("stale-digest").try_into().unwrap())
                .unwrap(),
            server_info_refreshed_at: Instant::now()
                - DEFAULT_SERVER_INFO_TTL
                - Duration::from_secs(1),
            contract_manager: Mutex::new(contract_manager),
        }));
        let hook_state = cached_state.clone();
        inner.set_info_refresh_hook(move |server_info| {
            update_server_state(&hook_state, server_info)
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
        });

        let err = match inner
            .list_vtxos(GetVtxosRequest::new_for_outpoints(&[OutPoint::null()]))
            .await
        {
            Ok(_) => panic!("list_vtxos unexpectedly succeeded"),
            Err(err) => err,
        };

        assert!(err.is_server_info_changed());
        assert!(Error::from(err).is_server_info_changed());
        assert_eq!(state.list_vtxos_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.get_info_calls.load(Ordering::SeqCst), 1);
        let refreshed_state = cached_state.read().unwrap();
        assert_eq!(refreshed_state.server_info.digest, "fresh-digest");
        assert!(refreshed_state.server_info_refreshed_at.elapsed() < DEFAULT_SERVER_INFO_TTL);
    }
}
