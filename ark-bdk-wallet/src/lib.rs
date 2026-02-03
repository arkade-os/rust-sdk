use anyhow::Result;
use ark_client::error::Error;
use ark_client::error::ErrorContext;
use ark_client::wallet::Balance;
use ark_client::wallet::OnchainWallet;
use ark_client::wallet::Persistence;
use ark_core::BoardingOutput;
use ark_core::SelectedUtxo;
use ark_core::UtxoCoinSelection;
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::KeychainKind;
use bdk_wallet::SignOptions;
use bdk_wallet::TxOrdering;
use bdk_wallet::Wallet as BdkWallet;
use bitcoin::bip32::Xpriv;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::FeeRate;
use bitcoin::Network;
use bitcoin::Psbt;
use bitcoin::XOnlyPublicKey;
use jiff::Timestamp;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;
use std::sync::RwLock;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod utils;

pub struct Wallet<DB>
where
    DB: Persistence,
{
    inner: Arc<RwLock<BdkWallet>>,
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    client: esplora_client::AsyncClient,
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    client: esplora_client::AsyncClient<WebSleeper>,
    db: DB,
}

impl<DB> Wallet<DB>
where
    DB: Persistence,
{
    pub fn new(
        kp: Keypair,
        _secp: Secp256k1<All>,
        network: Network,
        esplora_url: &str,
        db: DB,
    ) -> Result<Self> {
        let key = kp.secret_key();
        let xprv = Xpriv::new_master(network, key.as_ref())?;
        let external = bdk_wallet::template::Bip84(xprv, KeychainKind::External);
        let change = bdk_wallet::template::Bip84(xprv, KeychainKind::Internal);
        let wallet = BdkWallet::create(external, change)
            .network(network)
            .create_wallet_no_persist()?;

        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        let client = esplora_client::Builder::new(esplora_url).build_async_with_sleeper()?;

        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        let client =
            esplora_client::Builder::new(esplora_url).build_async_with_sleeper::<WebSleeper>()?;

        Ok(Self {
            inner: Arc::new(RwLock::new(wallet)),
            client,
            db,
        })
    }
}

impl<DB> OnchainWallet for Wallet<DB>
where
    DB: Persistence + Send + Sync,
{
    fn get_onchain_address(&self) -> Result<Address, Error> {
        let info = self
            .inner
            .write()
            .map_err(|e| Error::consumer(format!("failed to get write lock: {e}")))?
            .next_unused_address(KeychainKind::External);

        Ok(info.address)
    }

    async fn sync(&self) -> Result<(), Error> {
        let request = self
            .inner
            .read()
            .map_err(|e| Error::consumer(format!("failed to get read lock: {e}")))?
            .start_full_scan()
            .inspect({
                let mut stdout = std::io::stdout();
                let mut once = BTreeSet::<KeychainKind>::new();
                move |keychain, spk_i, _| {
                    if once.insert(keychain) {
                        tracing::trace!(?keychain, "Scanning keychain");
                    }
                    tracing::trace!(" {:<3}", spk_i);
                    stdout.flush().expect("must flush")
                }
            });

        let now: std::time::Duration = Timestamp::now()
            .as_duration()
            .try_into()
            .map_err(Error::wallet)?;

        // TODO: Use smarter constants or make it configurable.
        let update = self
            .client
            .full_scan(request, 5, 5)
            .await
            .map_err(Error::wallet)
            .context("Failed syncing wallet")?;

        self.inner
            .write()
            .expect("write lock")
            .apply_update_at(update, now.as_secs())
            .map_err(Error::wallet)?;

        Ok(())
    }

    fn balance(&self) -> Result<Balance, Error> {
        let balance = self
            .inner
            .read()
            .map_err(|e| Error::consumer(format!("failed to get read lock: {e}")))?
            .balance();

        Ok(Balance {
            immature: balance.immature,
            trusted_pending: balance.trusted_pending,
            untrusted_pending: balance.untrusted_pending,
            confirmed: balance.confirmed,
        })
    }

    fn prepare_send_to_address(
        &self,
        address: Address,
        amount: Amount,
        fee_rate: FeeRate,
    ) -> Result<Psbt, Error> {
        let wallet = &mut self
            .inner
            .write()
            .map_err(|e| Error::consumer(format!("failed to get write lock: {e}")))?;
        let mut b = wallet.build_tx();
        b.ordering(TxOrdering::Untouched);
        b.add_recipient(address.script_pubkey(), amount);
        b.fee_rate(fee_rate);

        let psbt = b.finish().map_err(Error::wallet)?;

        Ok(psbt)
    }

    fn sign(&self, psbt: &mut Psbt) -> Result<bool, Error> {
        let options = SignOptions {
            trust_witness_utxo: true,
            ..SignOptions::default()
        };

        let finalized = self
            .inner
            .read()
            .map_err(|e| Error::consumer(format!("failed to get read lock: {e}")))?
            .sign(psbt, options)
            .map_err(Error::wallet)?;

        Ok(finalized)
    }

    fn select_coins(&self, target_amount: Amount) -> Result<UtxoCoinSelection, Error> {
        let wallet = self
            .inner
            .read()
            .map_err(|e| Error::consumer(format!("failed to get read lock: {e}")))?;

        // Get all unspent UTXOs
        let utxos = wallet.list_unspent();

        // Simple coin selection: pick UTXOs until we reach the target amount
        let mut selected_utxos = Vec::new();
        let mut total_selected = Amount::ZERO;

        for utxo in utxos {
            if total_selected >= target_amount {
                break;
            }

            // Get the address for this UTXO
            let address = wallet
                .peek_address(utxo.keychain, utxo.derivation_index)
                .address;

            selected_utxos.push(SelectedUtxo {
                outpoint: utxo.outpoint,
                amount: utxo.txout.value,
                address,
            });

            total_selected += utxo.txout.value;
        }

        if total_selected < target_amount {
            return Err(Error::wallet(format!(
                "Insufficient funds: need {target_amount}, have {total_selected}"
            )));
        }

        let change_amount = total_selected - target_amount;

        Ok(UtxoCoinSelection {
            selected_utxos,
            total_selected,
            change_amount,
        })
    }
}

impl<DB> Persistence for Wallet<DB>
where
    DB: Persistence,
{
    fn save_boarding_output(
        &self,
        sk: bitcoin::secp256k1::SecretKey,
        boarding_output: BoardingOutput,
    ) -> Result<(), Error> {
        self.db.save_boarding_output(sk, boarding_output)
    }

    fn load_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, Error> {
        self.db.load_boarding_outputs()
    }

    fn sk_for_pk(&self, pk: &XOnlyPublicKey) -> Result<bitcoin::secp256k1::SecretKey, Error> {
        self.db.sk_for_pk(pk)
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[derive(Clone)]
struct WebSleeper;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl esplora_client::Sleeper for WebSleeper {
    type Sleep = utils::SendWrapper<gloo_timers::future::TimeoutFuture>;

    fn sleep(dur: std::time::Duration) -> Self::Sleep {
        utils::SendWrapper(gloo_timers::future::sleep(dur))
    }
}
