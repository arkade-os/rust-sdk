use crate::error::ErrorContext;
use crate::key_provider::KeypairIndex;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use ark_core::asset::AssetId;
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
use ark_core::BoardingOutput;
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
use std::sync::RwLock;
use std::time::Duration;

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
mod send_vtxo;
mod unilateral_exit;
mod utils;

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

/// Default Boltz `referralId` sent with swap creation requests when the caller does not
/// provide one. Identifies traffic originating from this SDK.
pub const DEFAULT_BOLTZ_REFERRAL_ID: &str = "arkade-rs-SDK";

/// Maximum number of inputs a single deprecated-signer migration leg will settle in one batch.
///
/// A client-side safeguard mirroring ts-sdk's `MAX_VTXOS_PER_SETTLEMENT`: it bounds the input
/// count of one [`Client::migrate_deprecated_signer_vtxos`] leg so a wallet holding many small
/// VTXOs does not build a batch intent that exceeds the server's transaction-weight limit. Any
/// overflow is deferred to a later migration cycle (see [`MigrationLegReport::deferred`]).
pub const MAX_VTXOS_PER_SETTLEMENT: usize = 50;

/// Mainnet's original unilateral-exit delay (~7 days, in seconds).
///
/// arkd only advertises the CURRENT exit delay in `/info`; it does not record the delays it used
/// in the past. If the operator shortens the delay (with or without a key rotation), deposits
/// minted under the OLD delay have a different `scriptPubKey` and would silently fall out of
/// watch/discovery. To avoid that, discovery on mainnet probes this hardcoded legacy delay
/// alongside the advertised one. We keep a single fallback rather than scanning an unbounded
/// history because arkd does not expose deprecated delays. Matches `MAINNET_UNILATERAL_EXIT_DELAY`
/// in arkade-os/ts-sdk and `MainnetLegacyUnilateralExit` in the dotnet SDK.
///
/// On testnets arkd has always advertised the network's intended delay, so this probe would only
/// widen the candidate set without ever hitting; the candidate-delay helper therefore adds it on
/// mainnet only (see [`Client::candidate_exit_delays`]).
const MAINNET_LEGACY_UNILATERAL_EXIT_DELAY_SECS: u32 = 605_184;

/// A single VTXO or boarding output referenced in a [`DeprecatedSignerMigrationReport`].
#[derive(Debug, Clone)]
pub struct MigrationVtxoRef {
    /// The input's outpoint.
    pub outpoint: OutPoint,
    /// The input's amount.
    pub amount: Amount,
    /// The deprecated signer the input was minted under.
    pub signer_pk: XOnlyPublicKey,
    /// The signer's advertised cooperative-sign cutoff (Unix seconds); `0` means "rotate now".
    pub cutoff_date: i64,
}

/// Why a single migration leg ([`DeprecatedSignerMigrationReport::vtxo`] or
/// [`DeprecatedSignerMigrationReport::boarding`]) settled nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationSkipReason {
    /// The selected aggregate fell below the server's dust floor.
    BelowDust,
    /// Every migratable input in the leg individually exceeds the per-output ceiling
    /// (`vtxo_max_amount`); none can migrate cooperatively, so the leg has only `oversized`
    /// inputs and submitted nothing.
    OversizedOnly,
    /// The leg had no migratable inputs at all.
    NothingMigratable,
}

/// Outcome of one [`Client::migrate_deprecated_signer_vtxos`] leg.
///
/// Each leg owns its full sizing pipeline and reports independently — a failure or skip in one leg
/// never suppresses the other. The pipeline (mirroring ts-sdk's `runMigrationLeg`) is:
///
/// 1. inputs whose individual amount exceeds the server's per-output ceiling (`vtxo_max_amount`)
///    are split out as [`Self::oversized`] — they can never form a `<= ceiling` output and must
///    exit unilaterally;
/// 2. the remainder is selected highest-value-first, bounded by both [`MAX_VTXOS_PER_SETTLEMENT`]
///    and a running aggregate within the ceiling — the overflow lands in [`Self::deferred`] for a
///    later cycle;
/// 3. if the selected aggregate is below the dust floor, the leg is [`Self::skipped`] and nothing
///    is submitted.
#[derive(Debug, Clone)]
pub struct MigrationLegReport {
    /// The settlement TXID, when this leg submitted a batch. `None` on skip.
    pub settle_txid: Option<Txid>,
    /// Inputs submitted in this leg's settlement; empty on skip.
    pub migrated: Vec<MigrationVtxoRef>,
    /// Migratable inputs deferred to a later cycle by this leg's count or amount caps.
    pub deferred: Vec<MigrationVtxoRef>,
    /// Inputs whose value alone exceeds the per-output ceiling; they require a unilateral exit and
    /// never migrate cooperatively.
    pub oversized: Vec<MigrationVtxoRef>,
    /// Why this leg submitted nothing; `None` when a settlement was attempted.
    pub skipped: Option<MigrationSkipReason>,
    /// The settlement error, if this leg's `settle_vtxos` call failed. Set independently of the
    /// other leg — a failure here does not prevent the other leg from running.
    pub error: Option<String>,
}

impl MigrationLegReport {
    /// A leg that submitted nothing for the given reason.
    fn skipped(reason: MigrationSkipReason) -> Self {
        Self {
            settle_txid: None,
            migrated: Vec::new(),
            deferred: Vec::new(),
            oversized: Vec::new(),
            skipped: Some(reason),
            error: None,
        }
    }
}

/// Result of a [`Client::migrate_deprecated_signer_vtxos`] pass, split into two symmetric legs:
/// a VTXO leg and a boarding leg. They are never combined into a single intent.
#[derive(Debug, Clone)]
pub struct DeprecatedSignerMigrationReport {
    /// The VTXO migration leg.
    pub vtxo: MigrationLegReport,
    /// The boarding-output migration leg.
    pub boarding: MigrationLegReport,
}

impl DeprecatedSignerMigrationReport {
    /// A report where both legs found nothing to migrate (e.g. the server advertises no
    /// deprecated signers, or the wallet holds no pre-cutoff deprecated-signer outputs).
    fn nothing_migratable() -> Self {
        Self {
            vtxo: MigrationLegReport::skipped(MigrationSkipReason::NothingMigratable),
            boarding: MigrationLegReport::skipped(MigrationSkipReason::NothingMigratable),
        }
    }

    /// Whether the wallet was rotated off a deprecated signer this pass — i.e. at least one leg
    /// submitted a settlement.
    pub fn rotated(&self) -> bool {
        self.vtxo.settle_txid.is_some() || self.boarding.settle_txid.is_some()
    }

    /// The settlement TXIDs produced this pass (at most one per leg).
    pub fn settle_txids(&self) -> Vec<Txid> {
        [self.vtxo.settle_txid, self.boarding.settle_txid]
            .into_iter()
            .flatten()
            .collect()
    }
}

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
///         None,
///         timeout,
///         None,
///         vec![],
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
///         None,
///         timeout,
///         None,
///         vec![],
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
    boltz_referral_id: Option<String>,
    timeout: Duration,
    delegator_pk: Option<XOnlyPublicKey>,
    historical_delegator_pks: Vec<XOnlyPublicKey>,
}

/// A client to interact with Ark server
///
/// See [`OfflineClient`] docs for details.
pub struct Client<B, W, S, K> {
    inner: OfflineClient<B, W, S, K>,
    state: Arc<RwLock<ServerState>>,
}

struct ServerState {
    server_info: server::Info,
    fee_estimator: ark_fees::Estimator,
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

/// Current time as unix seconds. Uses `js_sys::Date` on wasm32, `std::time` elsewhere.
fn unix_now() -> i64 {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("valid duration")
            .as_secs() as i64
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }
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
    /// * `boltz_referral_id` - Boltz referral ID to be included in all swap creation requests as
    ///   the `referralId` field. When `None`, defaults to [`DEFAULT_BOLTZ_REFERRAL_ID`]. To send no
    ///   referral ID at all, call [`OfflineClient::with_boltz_referral_id`] with `None` after
    ///   construction.
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
        boltz_referral_id: Option<String>,
        timeout: Duration,
        delegator_pk: Option<XOnlyPublicKey>,
        historical_delegator_pks: Vec<XOnlyPublicKey>,
    ) -> Self {
        let secp = Secp256k1::new();

        let network_client = ark_grpc::Client::new(ark_server_url);

        // Normalize historical delegator keys once (preserve order, remove duplicates), then
        // ensure the current delegator key is present at the front.
        let mut seen = HashSet::new();
        let mut historical_delegator_pks: Vec<_> = historical_delegator_pks
            .into_iter()
            .filter(|pk| seen.insert(*pk))
            .collect();

        if let Some(pk) = delegator_pk {
            historical_delegator_pks.retain(|k| *k != pk);
            historical_delegator_pks.insert(0, pk);
        }

        let boltz_referral_id =
            boltz_referral_id.or_else(|| Some(DEFAULT_BOLTZ_REFERRAL_ID.to_string()));

        Self {
            network_client,
            name,
            key_provider,
            blockchain,
            secp,
            wallet,
            swap_storage,
            boltz_url,
            boltz_referral_id,
            timeout,
            delegator_pk,
            historical_delegator_pks,
        }
    }

    /// Override the Boltz referral ID after construction.
    ///
    /// Pass `Some(...)` to set a custom value, or `None` to send no `referralId` field with
    /// swap creation requests (this opts out of the SDK default).
    pub fn with_boltz_referral_id(mut self, boltz_referral_id: Option<String>) -> Self {
        self.boltz_referral_id = boltz_referral_id;
        self
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
        boltz_referral_id: Option<String>,
        timeout: Duration,
        delegator_pk: Option<XOnlyPublicKey>,
        historical_delegator_pks: Vec<XOnlyPublicKey>,
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
            boltz_referral_id,
            timeout,
            delegator_pk,
            historical_delegator_pks,
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
        boltz_referral_id: Option<String>,
        timeout: Duration,
        delegator_pk: Option<XOnlyPublicKey>,
        historical_delegator_pks: Vec<XOnlyPublicKey>,
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
            boltz_referral_id,
            timeout,
            delegator_pk,
            historical_delegator_pks,
        )
    }

    /// Returns the currently configured delegator pubkey, if any.
    pub fn delegator_pk(&self) -> Option<XOnlyPublicKey> {
        self.delegator_pk
    }

    /// Returns the Boltz referral ID sent with all swap creation requests, if any.
    pub fn boltz_referral_id(&self) -> Option<&str> {
        self.boltz_referral_id.as_deref()
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

        let fee_estimator = build_fee_estimator(&server_info)?;
        let state = Arc::new(RwLock::new(ServerState {
            server_info,
            fee_estimator,
        }));
        let hook_state = state.clone();
        self.network_client
            .set_info_refresh_hook(move |server_info| {
                update_server_state(&hook_state, server_info)
                    .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
            });

        let client = Client { inner: self, state };

        if let Err(error) = client.discover_keys(DEFAULT_GAP_LIMIT).await {
            tracing::warn!(?error, "Failed during key discovery");
        };

        // Eagerly persist boarding rows for the current signer and every deprecated signer (each
        // crossed with the candidate exit delays). Without this, deprecated-signer boarding rows
        // exist only after an integrator calls `get_boarding_addresses()`, and the boarding leg of
        // `migrate_deprecated_signer_vtxos` (a pure DB read) would silently see none — an ordering
        // footgun the ts-sdk (eager boarding matrix at boot) and dotnet (BoardingUtxoSyncService)
        // both avoid. Re-persisting is idempotent, so this is safe to run on every connect.
        match client.server_info() {
            Ok(server_info) => {
                if let Err(error) = client.persist_watch_boarding_outputs(&server_info) {
                    tracing::warn!(?error, "Failed to persist boarding outputs at connect");
                }
            }
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "Failed to read server info for boarding persistence"
                );
            }
        }

        Ok(client)
    }
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
    Ok(())
}

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: KeyProvider,
{
    /// Returns the latest cached Ark server info.
    pub fn server_info(&self) -> Result<server::Info, Error> {
        self.state
            .read()
            .map(|state| state.server_info.clone())
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

    /// Refresh cached `/info` data after the server reports a digest mismatch.
    ///
    /// This updates server info, the gRPC digest header, and the fee estimator. The SDK
    /// intentionally does not retry the failed operation automatically; rebuild the request using
    /// the refreshed server info and retry only when it is safe for your call site.
    pub async fn refresh_server_info(&self) -> Result<(), Error> {
        timeout_op(
            self.inner.timeout,
            self.network_client().get_info_unguarded(),
        )
        .await
        .context("Failed to refresh Ark server info")??;

        Ok(())
    }

    /// Refresh cached `/info` if `error` is a digest mismatch.
    ///
    /// Returns `true` when a refresh happened. The original operation is not retried; callers must
    /// rebuild any request state that depended on the old [`Client::server_info`] before retrying.
    pub async fn refresh_server_info_if_digest_mismatch(
        &self,
        error: &Error,
    ) -> Result<bool, Error> {
        if !error.is_digest_mismatch() {
            return Ok(false);
        }

        self.refresh_server_info().await?;
        Ok(true)
    }

    /// Returns the currently configured delegator pubkey, if any.
    pub fn delegator_pk(&self) -> Option<XOnlyPublicKey> {
        self.inner.delegator_pk()
    }

    /// Returns the Boltz referral ID sent with all swap creation requests, if any.
    pub fn boltz_referral_id(&self) -> Option<&str> {
        self.inner.boltz_referral_id()
    }

    /// Get a new offchain receiving address.
    ///
    /// When a delegator is configured (via `delegator_pk` passed to [`OfflineClient::new`]),
    /// returns a 3-leaf delegate address. Otherwise returns a standard 2-leaf address.
    ///
    /// For HD wallets, this will derive a new address each time it's called.
    /// For static key providers, this will always return the same address.
    pub fn get_offchain_address(&self) -> Result<(ArkAddress, Vtxo), Error> {
        let server_info = &self.server_info()?;

        let server_signer = server_info.signer_pk.into();
        let owner = self
            .next_keypair(KeypairIndex::LastUnused)?
            .public_key()
            .into();

        let vtxo = self.make_vtxo(server_signer, owner)?;

        let ark_address = vtxo.to_ark_address();

        Ok((ark_address, vtxo))
    }

    /// Get all known offchain addresses for this wallet.
    ///
    /// When a delegator is configured, this returns **both** the default (2-leaf) and delegate
    /// (3-leaf) addresses for each key, so that VTXOs at either address are visible. If
    /// historical delegator keys are set via `historical_delegator_pks` passed to
    /// [`OfflineClient::new`], addresses for those are included too.
    pub fn get_offchain_addresses(&self) -> Result<Vec<(ArkAddress, Vtxo)>, Error> {
        let server_info = &self.server_info()?;
        let pks = self.inner.key_provider.get_cached_pks()?;

        // Build addresses for current signer + all deprecated signers so VTXOs under any
        // known server key are discovered and visible in the balance.
        let all_server_keys: Vec<XOnlyPublicKey> = server_info.all_server_keys().collect();

        let mut results = Vec::new();

        for owner_pk in &pks {
            for server_signer in &all_server_keys {
                // Default (2-leaf) address.
                let default_vtxo = Vtxo::new_default(
                    self.secp(),
                    *server_signer,
                    *owner_pk,
                    server_info.unilateral_exit_delay,
                    server_info.network,
                )?;
                results.push((default_vtxo.to_ark_address(), default_vtxo));

                // Delegate addresses for all known delegator keys.
                let mut seen = HashSet::new();
                for dpk in &self.inner.historical_delegator_pks {
                    if !seen.insert(dpk) {
                        continue;
                    }
                    let delegate_vtxo = Vtxo::new_with_delegator(
                        self.secp(),
                        *server_signer,
                        *owner_pk,
                        *dpk,
                        server_info.unilateral_exit_delay,
                        server_info.network,
                    )?;
                    results.push((delegate_vtxo.to_ark_address(), delegate_vtxo));
                }
            }
        }

        Ok(results)
    }

    /// Build a [`Vtxo`] for the given owner key, using a 3-leaf delegate VTXO if a delegator is
    /// configured, otherwise a standard 2-leaf default VTXO.
    fn make_vtxo(
        &self,
        server_signer: XOnlyPublicKey,
        owner: XOnlyPublicKey,
    ) -> Result<Vtxo, Error> {
        let server_info = &self.server_info()?;
        match self.inner.delegator_pk {
            Some(delegator) => Vtxo::new_with_delegator(
                self.secp(),
                server_signer,
                owner,
                delegator,
                server_info.unilateral_exit_delay,
                server_info.network,
            )
            .map_err(Into::into),
            None => Vtxo::new_default(
                self.secp(),
                server_signer,
                owner,
                server_info.unilateral_exit_delay,
                server_info.network,
            )
            .map_err(Into::into),
        }
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

        let server_info = &self.server_info()?;
        // Discover against current + all deprecated signers so that user keys used before a
        // rotation are still found when recovering from seed.
        let all_server_keys: Vec<XOnlyPublicKey> = server_info.all_server_keys().collect();
        // Probe each server key under every candidate exit delay (the advertised delay plus, on
        // mainnet, the legacy delay), so VTXOs minted before the operator shortened the delay are
        // still discovered. Off mainnet this is just the advertised delay (no behaviour change).
        let candidate_delays =
            self.candidate_exit_delays(server_info.unilateral_exit_delay, server_info.network)?;

        let mut start_index = 0u32;
        let mut discovered_count = 0u32;

        tracing::info!(gap_limit, "Starting key discovery");

        loop {
            // Generate a batch of gap_limit keys
            let mut batch: Vec<(u32, Keypair, Vec<ArkAddress>)> =
                Vec::with_capacity(gap_limit as usize);

            for i in 0..gap_limit {
                let index = start_index
                    .checked_add(i)
                    .ok_or_else(|| Error::ad_hoc("Key discovery index overflow"))?;

                let kp = match self.inner.key_provider.derive_at_discovery_index(index)? {
                    Some(kp) => kp,
                    None => break,
                };

                let owner_pk = kp.x_only_public_key().0;

                let mut addresses = Vec::new();

                for server_signer in &all_server_keys {
                    for exit_delay in &candidate_delays {
                        // Default (2-leaf) address.
                        let default_vtxo = Vtxo::new_default(
                            self.secp(),
                            *server_signer,
                            owner_pk,
                            *exit_delay,
                            server_info.network,
                        )?;
                        addresses.push(default_vtxo.to_ark_address());

                        // Delegate (3-leaf) addresses for each known delegator.
                        for dpk in &self.inner.historical_delegator_pks {
                            let delegate_vtxo = Vtxo::new_with_delegator(
                                self.secp(),
                                *server_signer,
                                owner_pk,
                                *dpk,
                                *exit_delay,
                                server_info.network,
                            )?;
                            addresses.push(delegate_vtxo.to_ark_address());
                        }
                    }
                }

                batch.push((index, kp, addresses));
            }

            if batch.is_empty() {
                break;
            }

            // Query all addresses in batch at once
            let addresses = batch.iter().flat_map(|(_, _, addrs)| addrs.iter().copied());

            let vtxo_list = self.list_vtxos_for_addresses(addresses).await?;

            // Build set of used scripts from response
            let used_scripts: HashSet<&ScriptBuf> = vtxo_list.all().map(|v| &v.script).collect();

            // Cache keypairs for used addresses (match by script)
            let mut found_any = false;
            for (index, kp, addrs) in batch {
                let used_addr = addrs.iter().find(|addr| {
                    let script = addr.to_p2tr_script_pubkey();
                    used_scripts.contains(&script)
                });
                if let Some(addr) = used_addr {
                    tracing::debug!(index, addr = %addr, "Found used address");
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

    /// Candidate exit-delay set for discovery/watch, given the current delay advertised for one
    /// axis (boarding or unilateral-exit).
    ///
    /// Returns the `current` delay plus — only on mainnet — the hardcoded legacy delay
    /// [`MAINNET_LEGACY_UNILATERAL_EXIT_DELAY_SECS`], deduplicated. arkd advertises only the
    /// CURRENT delay, so deposits minted under an older (longer) delay live at a different
    /// `scriptPubKey`; probing the legacy delay too keeps them visible. On non-mainnet the set is
    /// just `[current]`, so behaviour there is unchanged, and the de-dup makes the legacy entry a
    /// no-op whenever `current` already equals it.
    fn candidate_exit_delays(
        &self,
        current: bitcoin::Sequence,
        network: bitcoin::Network,
    ) -> Result<Vec<bitcoin::Sequence>, Error> {
        let mut delays = vec![current];

        if network == bitcoin::Network::Bitcoin {
            let legacy =
                bitcoin::Sequence::from_seconds_ceil(MAINNET_LEGACY_UNILATERAL_EXIT_DELAY_SECS)
                    .map_err(Error::ad_hoc)?;
            // De-dup: when the advertised mainnet delay already equals the legacy value, the
            // legacy probe is a no-op.
            if !delays.contains(&legacy) {
                delays.push(legacy);
            }
        }

        Ok(delays)
    }

    // At the moment we are always generating the same address.
    pub fn get_boarding_address(&self) -> Result<Address, Error> {
        let server_info = &self.server_info()?;

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
        let server_info = &self.server_info()?;

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
    /// with each candidate exit delay, returning the created [`BoardingOutput`]s.
    ///
    /// Covers the current signer plus every deprecated signer, each paired with
    /// [`Client::candidate_exit_delays`] (the advertised boarding-exit delay plus, on mainnet, the
    /// legacy delay). Calling [`BoardingWallet::new_boarding_output`] writes the row to the
    /// wallet's store; re-persisting the same boarding output is a harmless overwrite (the store
    /// keys rows by [`BoardingOutput`] identity), so this is safe to call repeatedly — at connect
    /// time and again from [`Client::get_boarding_addresses`].
    fn persist_watch_boarding_outputs(
        &self,
        server_info: &server::Info,
    ) -> Result<Vec<BoardingOutput>, Error> {
        let candidate_delays =
            self.candidate_exit_delays(server_info.boarding_exit_delay, server_info.network)?;

        let mut outputs = Vec::new();
        for server_pk in server_info.all_server_keys() {
            for exit_delay in &candidate_delays {
                let boarding_output = self.inner.wallet.new_boarding_output(
                    server_pk,
                    *exit_delay,
                    server_info.network,
                )?;
                outputs.push(boarding_output);
            }
        }

        Ok(outputs)
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

        let vtxo_list = VtxoList::new(self.server_info()?.dust, virtual_tx_outpoints);

        Ok(vtxo_list)
    }

    pub async fn list_vtxos_for_outpoints(
        &self,
        outpoints: Vec<OutPoint>,
    ) -> Result<(VtxoList, HashMap<ScriptBuf, Vtxo>), Error> {
        let ark_addresses = self.get_offchain_addresses()?;

        let script_pubkey_to_vtxo_map = ark_addresses
            .iter()
            .map(|(a, v)| (a.to_p2tr_script_pubkey(), v.clone()))
            .collect::<HashMap<_, _>>();

        let request = GetVtxosRequest::new_for_outpoints(&outpoints);
        let virtual_tx_outpoints = self.fetch_all_vtxos(request).await?;

        // Filter out outpoints for which we don't have spend info.
        let virtual_tx_outpoints = virtual_tx_outpoints
            .into_iter()
            .filter(|v| match script_pubkey_to_vtxo_map.get(&v.script) {
                Some(_) => true,
                None => {
                    tracing::debug!(outpoint = %v.outpoint, "Missing spend info for VTXO");

                    false
                }
            })
            .collect();

        let vtxo_list = VtxoList::new(self.server_info()?.dust, virtual_tx_outpoints);

        Ok((vtxo_list, script_pubkey_to_vtxo_map))
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
        let (vtxo_list, script_map) = self.list_vtxos().await.context("failed to list VTXOs")?;
        let now = unix_now();
        // Snapshot once so every bucket is classified against the same signer set.
        let server_info = self.server_info()?;

        let is_past_cutoff = |v: &VirtualTxOutPoint| {
            script_map
                .get(&v.script)
                .map(|vtxo| server_info.is_signer_past_cutoff_at(vtxo.server_pk(), now))
                .unwrap_or(false)
        };

        let pre_confirmed = vtxo_list
            .pre_confirmed()
            .filter(|v| !is_past_cutoff(v))
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        let confirmed = vtxo_list
            .confirmed()
            .filter(|v| !is_past_cutoff(v))
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        let recoverable = vtxo_list
            .recoverable()
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        // Spendable offchain VTXOs under a past-cutoff deprecated signer: operator won't
        // co-sign, so they're stuck until the VTXO expires and becomes recoverable.
        let pending_recovery = vtxo_list
            .spendable_offchain()
            .filter(|v| is_past_cutoff(v))
            .fold(Amount::ZERO, |acc, x| acc + x.amount);

        // Aggregate asset balances from spendable (non-past-cutoff) VTXOs only.
        let mut asset_balances: HashMap<AssetId, u64> = HashMap::new();
        for vtxo in vtxo_list
            .spendable_offchain()
            .filter(|v| !is_past_cutoff(v))
        {
            for asset in &vtxo.assets {
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

    /// Sweep VTXOs and boarding outputs minted under a *pre-cutoff* deprecated server signer to
    /// the current signer, then report what moved.
    ///
    /// Only deprecated-signer, pre-cutoff inputs are touched — current-signer outputs are left
    /// untouched (no consolidation, no incidental settlement fee), and past-cutoff outputs are
    /// skipped automatically by [`Self::fetch_commitment_transaction_inputs`] (the operator won't
    /// co-sign the old key, so they become recoverable after expiry and exit via the recovery
    /// path).
    ///
    /// Migration runs as two **independent** legs — a VTXO leg and a boarding leg — each routed
    /// through [`Self::settle_vtxos`] with its own scoped outpoint set. A failure in one leg does
    /// not suppress the other. Before settling, each leg is sized against the server's per-output
    /// ceiling (`vtxo_max_amount`) and dust floor (see [`MigrationLegReport`] for the exact
    /// pipeline): inputs that individually exceed the ceiling are reported as `oversized` (they can
    /// never form a `<= ceiling` output and must exit unilaterally — they are NOT silently
    /// dropped); the remainder is selected highest-value-first up to [`MAX_VTXOS_PER_SETTLEMENT`]
    /// and a running aggregate within the ceiling, deferring the rest to a later cycle; a leg whose
    /// selected aggregate is below dust is skipped.
    ///
    /// When the server advertises no deprecated signers, returns an empty
    /// [`MigrationSkipReason::NothingMigratable`] report without touching the wallet.
    pub async fn migrate_deprecated_signer_vtxos<R>(
        &self,
        rng: &mut R,
    ) -> Result<DeprecatedSignerMigrationReport, Error>
    where
        R: rand::Rng + rand::CryptoRng + Clone,
    {
        // Snapshot the server info once (TOCTOU): the empty-check, the per-input
        // classification closure, and the leg sizing must all see the same
        // `deprecated_signers`/`vtxo_max_amount`/`dust` even if a concurrent digest-driven
        // `refresh_server_info` swaps the snapshot mid-call.
        let server_info = self.server_info()?;
        if server_info.deprecated_signers.is_empty() {
            return Ok(DeprecatedSignerMigrationReport::nothing_migratable());
        }

        let now = unix_now();

        let is_pre_cutoff_deprecated = |server_pk: XOnlyPublicKey| -> Option<i64> {
            server_info
                .deprecated_signers
                .iter()
                .find(|ds| {
                    ds.pk.x_only_public_key().0 == server_pk
                        && (ds.cutoff_date == 0 || ds.cutoff_date > now)
                })
                .map(|ds| ds.cutoff_date)
        };

        // `fetch_commitment_transaction_inputs` already drops PAST-cutoff deprecated inputs (the
        // operator won't co-sign the old key). We narrow further to the PRE-cutoff deprecated
        // inputs, which is exactly the cooperatively-migratable set.
        let (boarding_inputs, vtxo_inputs, _) =
            self.fetch_commitment_transaction_inputs(now).await?;

        // The VTXO inputs only expose their script pubkey, so resolve each one's signer via the
        // script -> VTXO map (the same mapping `offchain_balance`/`settle_at` rely on).
        let (_, script_map) = self.list_vtxos().await?;

        // Build the candidate (outpoint, amount, signer, cutoff) list for the VTXO leg.
        let mut vtxo_candidates: Vec<MigrationVtxoRef> = Vec::new();
        for input in &vtxo_inputs {
            let Some(vtxo) = script_map.get(input.script_pubkey()) else {
                tracing::debug!(
                    outpoint = %input.outpoint(),
                    "Skipping VTXO with no spend info during migration"
                );
                continue;
            };
            if let Some(cutoff_date) = is_pre_cutoff_deprecated(vtxo.server_pk()) {
                vtxo_candidates.push(MigrationVtxoRef {
                    outpoint: input.outpoint(),
                    amount: input.amount(),
                    signer_pk: vtxo.server_pk(),
                    cutoff_date,
                });
            }
        }

        // Build the candidate list for the boarding leg.
        let mut boarding_candidates: Vec<MigrationVtxoRef> = Vec::new();
        for input in &boarding_inputs {
            let signer_pk = input.boarding_output().server_pk();
            if let Some(cutoff_date) = is_pre_cutoff_deprecated(signer_pk) {
                boarding_candidates.push(MigrationVtxoRef {
                    outpoint: input.outpoint(),
                    amount: input.amount(),
                    signer_pk,
                    cutoff_date,
                });
            }
        }

        if vtxo_candidates.is_empty() && boarding_candidates.is_empty() {
            tracing::debug!("No migratable deprecated-signer VTXOs or boarding outputs found");
            return Ok(DeprecatedSignerMigrationReport::nothing_migratable());
        }

        tracing::info!(
            num_vtxos = vtxo_candidates.len(),
            num_boarding = boarding_candidates.len(),
            "Found pre-cutoff deprecated-signer outputs; migrating to current signer"
        );

        let vtxo_max_amount = server_info.vtxo_max_amount;
        let dust = server_info.dust;

        // Run each leg independently so a failure in one does not suppress the other.
        let vtxo_leg = self
            .run_migration_leg(rng, vtxo_candidates, vtxo_max_amount, dust, true)
            .await?;
        let boarding_leg = self
            .run_migration_leg(rng, boarding_candidates, vtxo_max_amount, dust, false)
            .await?;

        Ok(DeprecatedSignerMigrationReport {
            vtxo: vtxo_leg,
            boarding: boarding_leg,
        })
    }

    /// Size a single migration leg against the server limits and settle the selected inputs.
    ///
    /// Mirrors ts-sdk's `runMigrationLeg`/`capSettlementBatch`. `is_vtxo_leg` selects which
    /// argument of [`Self::settle_vtxos`] the chosen outpoints are passed in (VTXO vs boarding);
    /// the other argument is empty so each leg is a distinct intent.
    async fn run_migration_leg<R>(
        &self,
        rng: &mut R,
        candidates: Vec<MigrationVtxoRef>,
        vtxo_max_amount: Option<Amount>,
        dust: Amount,
        is_vtxo_leg: bool,
    ) -> Result<MigrationLegReport, Error>
    where
        R: rand::Rng + rand::CryptoRng + Clone,
    {
        if candidates.is_empty() {
            return Ok(MigrationLegReport::skipped(
                MigrationSkipReason::NothingMigratable,
            ));
        }

        // (1) Split out inputs whose INDIVIDUAL amount exceeds the per-output ceiling. They can
        // never form a `<= ceiling` output, so they cannot migrate cooperatively and must exit
        // unilaterally. Report them rather than dropping them. `None` ceiling => no limit.
        let (oversized, mut sized): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|c| vtxo_max_amount.is_some_and(|max| c.amount > max));

        if !oversized.is_empty() {
            tracing::warn!(
                count = oversized.len(),
                ?vtxo_max_amount,
                "Deprecated-signer migration: inputs exceed the per-output limit and cannot be \
                 migrated cooperatively; they require a unilateral exit"
            );
        }

        // (2) Select highest-value-first, bounded by both the count cap and a running aggregate
        // within the ceiling. Skipped (not stopped) on an aggregate breach so a smaller input
        // behind an oversized-but-sized one still gets in; the count cap is a hard stop. The rest
        // is deferred to a later cycle.
        sized.sort_by_key(|c| std::cmp::Reverse(c.amount));

        let mut selected: Vec<MigrationVtxoRef> = Vec::new();
        let mut deferred: Vec<MigrationVtxoRef> = Vec::new();
        let mut aggregate = Amount::ZERO;
        for candidate in sized {
            if selected.len() >= MAX_VTXOS_PER_SETTLEMENT {
                deferred.push(candidate);
                continue;
            }
            let next = aggregate + candidate.amount;
            if vtxo_max_amount.is_some_and(|max| next > max) {
                deferred.push(candidate);
                continue;
            }
            aggregate = next;
            selected.push(candidate);
        }

        // (3) A migration output equals the gross sum of its inputs (migration is fee-exempt), so a
        // selected aggregate below dust would be rejected — skip the leg.
        if selected.is_empty() || aggregate < dust {
            // Nothing got selected and the only candidates were oversized => OversizedOnly;
            // otherwise the (sized) selection summed below dust.
            let reason = if selected.is_empty() && !oversized.is_empty() {
                MigrationSkipReason::OversizedOnly
            } else {
                MigrationSkipReason::BelowDust
            };
            return Ok(MigrationLegReport {
                settle_txid: None,
                migrated: Vec::new(),
                deferred,
                oversized,
                skipped: Some(reason),
                error: None,
            });
        }

        let selected_outpoints: Vec<OutPoint> = selected.iter().map(|c| c.outpoint).collect();
        let settle_result = if is_vtxo_leg {
            self.settle_vtxos(rng, &selected_outpoints, &[]).await
        } else {
            self.settle_vtxos(rng, &[], &selected_outpoints).await
        };

        // Capture (rather than propagate) the settle error so the caller can still run the other
        // leg — a failure in one leg must not suppress the other.
        Ok(match settle_result {
            Ok(settle_txid) => MigrationLegReport {
                settle_txid,
                migrated: selected,
                deferred,
                oversized,
                skipped: None,
                error: None,
            },
            Err(e) => {
                tracing::warn!(error = %e, "Deprecated-signer migration leg failed to settle");
                MigrationLegReport {
                    settle_txid: None,
                    migrated: Vec::new(),
                    // The selected inputs did not move; surface them as deferred so a retry
                    // re-attempts them.
                    deferred: selected.into_iter().chain(deferred).collect(),
                    oversized,
                    skipped: None,
                    error: Some(e.to_string()),
                }
            }
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

    /// The server's dust threshold amount.
    pub fn dust(&self) -> Result<Amount, Error> {
        Ok(self.server_info()?.dust)
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

    fn derivation_index_for_pk(&self, pk: &XOnlyPublicKey) -> Option<u32> {
        self.inner.key_provider.get_derivation_index_for_pk(pk)
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

    fn info_response(digest: &str) -> test_utils::GetInfoResponse {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
        let keypair = Keypair::from_secret_key(&secp, &secret_key);
        let public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let (xonly, _) = keypair.x_only_public_key();
        let address = Address::p2tr(&secp, xonly, None, bitcoin::Network::Regtest);

        test_utils::GetInfoResponse {
            version: "v0.9.8".to_string(),
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
        let cached_state = Arc::new(RwLock::new(ServerState {
            server_info: initial_info,
            fee_estimator: build_fee_estimator(&info_response("stale-digest").try_into().unwrap())
                .unwrap(),
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
        assert_eq!(
            cached_state.read().unwrap().server_info.digest,
            "fresh-digest"
        );
    }
}
