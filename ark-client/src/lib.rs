use crate::error::ErrorContext;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use ark_core::build_anchor_tx;
use ark_core::server;
use ark_core::server::VtxoOutPoint;
use ark_core::sort_transactions_by_created_at;
use ark_core::ArkAddress;
use ark_core::ArkNote;
use ark_core::ArkTransaction;
use ark_core::BoardingOutput;
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
use jiff::Timestamp;
use std::sync::Arc;

pub mod error;
pub mod round;
pub mod wallet;

mod coin_select;
mod send_vtxo;
mod unilateral_exit;
mod utils;

pub use error::Error;
pub use round::RoundOutputType;

/// A client to interact with Ark Server
///
/// ## Example
///
/// ```rust
/// # use std::future::Future;
/// # use std::str::FromStr;
/// # use ark_client::{Blockchain, Client, Error, ExplorerUtxo, SpendStatus};
/// # use ark_client::OfflineClient;
/// # use bitcoin::key::Keypair;
/// # use bitcoin::secp256k1::{Message, SecretKey};
/// # use std::sync::Arc;
/// # use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid, XOnlyPublicKey};
/// # use bitcoin::secp256k1::schnorr::Signature;
/// # use ark_client::wallet::{Balance, BoardingWallet, OnchainWallet, Persistence};
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
/// async fn init_client() -> Result<Client<MyBlockchain, MyWallet>, ark_client::Error> {
///     // Create a keypair for signing transactions
///     let secp = bitcoin::key::Secp256k1::new();
///     let secret_key = SecretKey::from_str("your_private_key_here").unwrap();
///     let keypair = Keypair::from_secret_key(&secp, &secret_key);
///
///     // Initialize blockchain and wallet implementations
///     let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
///     let wallet = Arc::new(MyWallet {});
///
///     // Create the offline client
///     let offline_client = OfflineClient::new(
///         "my-ark-client".to_string(),
///         keypair,
///         blockchain,
///         wallet,
///         "https://ark-server.example.com".to_string(),
///     );
///
///     // Connect to the Ark server and get server info
///     let client = offline_client.connect().await?;
///
///     Ok(client)
/// }
/// ```
pub struct OfflineClient<B, W> {
    // TODO: We could introduce a generic interface so that consumers can use either GRPC or REST.
    network_client: ark_grpc::Client,
    pub name: String,
    pub kp: Keypair,
    blockchain: Arc<B>,
    secp: Secp256k1<All>,
    wallet: Arc<W>,
}

/// A client to interact with Ark server
///
/// See [`OfflineClient`] docs for details.
pub struct Client<B, W> {
    inner: OfflineClient<B, W>,
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
    pub spendable: Vec<(Vec<VtxoOutPoint>, Vtxo)>,
    pub spent: Vec<(Vec<VtxoOutPoint>, Vtxo)>,
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

pub enum SettleInput {
    BoardingOutput(BoardingOutput),
    VtxoOutPoint(VtxoOutPoint),
    ArkNote(ArkNote),
}

pub struct SettleOutput {
    // FIXME: this is an ark address or we should have some other type?
    pub address: ArkAddress,
    pub amount: Amount,
}

// FIXME: move this somewhere else
pub struct SettleParams {
    // FIXME: what type should we use here? probably we should define a new one?
    // regarding the inputs probably we should have some sort of enum to aggregate boarding
    // outputs, vtxo and arknotes?
    pub inputs: Vec<SettleInput>,
    pub outputs: Vec<SettleOutput>,
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

impl<B, W> OfflineClient<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    pub fn new(
        name: String,
        kp: Keypair,
        blockchain: Arc<B>,
        wallet: Arc<W>,
        ark_server_url: String,
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
        }
    }

    // TODO: Add a timeout here. Maybe more generally.
    pub async fn connect(mut self) -> Result<Client<B, W>, Error> {
        self.network_client.connect().await?;
        let server_info = self.network_client.get_info().await?;

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

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    // At the moment we are always generating the same address.
    pub fn get_offchain_address(&self) -> Result<(ArkAddress, Vtxo), Error> {
        let server_info = &self.server_info;

        let (server, _) = server_info.pk.x_only_public_key();
        let (owner, _) = self.inner.kp.public_key().x_only_public_key();

        let vtxo = Vtxo::new(
            self.secp(),
            server,
            owner,
            vec![],
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
            server_info.pk.x_only_public_key().0,
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

    pub async fn list_vtxos(&self, select_recoverable_vtxos: bool) -> Result<ListVtxo, Error> {
        let addresses = self.get_offchain_addresses()?;

        let mut vtxos = ListVtxo {
            spendable: Vec::new(),
            spent: Vec::new(),
        };

        for (address, vtxo) in addresses.into_iter() {
            let list = self.network_client().list_vtxos(&address).await?;

            if select_recoverable_vtxos {
                vtxos
                    .spendable
                    .append(&mut vec![(list.spendable_with_recoverable(), vtxo.clone())]);

                vtxos
                    .spent
                    .append(&mut vec![(list.spent_without_recoverable(), vtxo.clone())]);
            } else {
                vtxos
                    .spendable
                    .append(&mut vec![(list.spendable().to_vec(), vtxo.clone())]);

                vtxos
                    .spent
                    .append(&mut vec![(list.spent().to_vec(), vtxo.clone())]);
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
        let vtxo_chain = self
            .network_client()
            .get_vtxo_chain(Some(out_point), Some((size, index)))
            .await?;

        Ok(Some(vtxo_chain))
    }

    // FIXME: make a typed return type for this!
    pub async fn redeem_notes(&self, arknotes: &[ArkNote]) -> Result<Txid, Error> {
        let (address, _) = self.get_offchain_address()?;
        let mut amount = Amount::from_sat(0);

        for arknote in arknotes {
            amount += arknote.value();
        }

        let rng = &mut rand::thread_rng();
        let txid = self
            .join_next_ark_round(
                rng,
                vec![],
                vec![],
                arknotes,
                RoundOutputType::Board {
                    to_address: address,
                    to_amount: amount,
                },
            )
            .await?;
        Ok(txid)
    }

    pub async fn spendable_vtxos(
        &self,
        select_recoverable_vtxos: bool,
    ) -> Result<Vec<(Vec<VtxoOutPoint>, Vtxo)>, Error> {
        let now = Timestamp::now();

        let mut spendable = vec![];

        let vtxos = self.list_vtxos(select_recoverable_vtxos).await?;

        for (virtual_tx_outpoints, vtxo) in vtxos.spendable {
            let explorer_utxos = self.blockchain().find_outpoints(vtxo.address()).await?;

            let mut spendable_outpoints = Vec::new();
            for vtxo_outpoint in virtual_tx_outpoints {
                match explorer_utxos
                    .iter()
                    .find(|explorer_utxo| explorer_utxo.outpoint == vtxo_outpoint.outpoint)
                {
                    // Include VTXOs that have been confirmed on the blockchain, but whose
                    // exit path is still _inactive_.
                    Some(ExplorerUtxo {
                        confirmation_blocktime: Some(confirmation_blocktime),
                        ..
                    }) if !vtxo.can_be_claimed_unilaterally_by_owner(
                        now.as_duration().try_into().map_err(Error::ad_hoc)?,
                        std::time::Duration::from_secs(*confirmation_blocktime),
                    ) =>
                    {
                        spendable_outpoints.push(vtxo_outpoint);
                    }
                    // The VTXO has not been confirmed on the blockchain yet. Therefore, it
                    // cannot have expired.
                    _ => {
                        spendable_outpoints.push(vtxo_outpoint);
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

    pub async fn transaction_history(&self) -> Result<Vec<ArkTransaction>, Error> {
        let mut boarding_transactions = Vec::new();
        let mut boarding_round_transactions = Vec::new();

        let boarding_addresses = self.get_boarding_addresses()?;
        for boarding_address in boarding_addresses.iter() {
            let outpoints = self.blockchain().find_outpoints(boarding_address).await?;

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

                let status = self
                    .blockchain()
                    .get_output_status(&outpoint.txid, outpoint.vout)
                    .await?;

                if let Some(spend_txid) = status.spend_txid {
                    boarding_round_transactions.push(spend_txid);
                }
            }
        }

        let offchain_addresses = self.get_offchain_addresses()?;

        let mut offchain_transactions = Vec::new();
        for (ark_address, _) in offchain_addresses.iter() {
            let txs = self.network_client().get_tx_history(ark_address).await?;

            for tx in txs {
                if !boarding_round_transactions.contains(&tx.txid()) {
                    offchain_transactions.push(tx);
                }
            }
        }

        let mut txs = [boarding_transactions, offchain_transactions].concat();

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

    #[cfg(feature = "e2e")]
    pub async fn make_arknote(&self, amount: Amount) -> Result<ArkNote, Error> {
        let note = self.network_client().create_note(amount, 1).await?;
        Ok(note)
    }

    /// Settle VTXOs and boarding outputs by consolidating them into new VTXOs.
    ///
    /// This method settles (consolidates) all available spendable VTXOs and boarding outputs
    /// into a single VTXO at the specified address. This is useful for cleaning up
    /// fragmented VTXOs and reducing the number of UTXOs you need to track.
    ///
    /// # Arguments
    ///
    /// * `rng` - Random number generator for cryptographic operations
    /// * `to_address` - The Ark address to settle all funds to
    /// * `select_recoverable_vtxos` - Whether to include recoverable VTXOs in settlement
    ///
    /// # Returns
    ///
    /// Returns the transaction ID of the settlement round if successful.
    /// Returns None if there are no inputs available for settlement.
    pub async fn settle(&self, _params: SettleParams) -> Result<Option<Txid>, Error> {
        unimplemented!()
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

    /// Use the P2A output of a transaction to bump its transaction fee with a child transaction.
    pub async fn bump_tx(&self, parent: &Transaction) -> Result<Transaction, Error> {
        let fee_rate = self.blockchain().get_fee_rate().await?;
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
}
