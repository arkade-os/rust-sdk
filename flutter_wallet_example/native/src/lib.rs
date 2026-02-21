use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Arc;

use ark_client::{Client, OfflineClient};
use ark_core::ArkAddress;
use bitcoin::key::Keypair;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::Address;

struct SimpleBlockchain;

#[async_trait::async_trait]
impl ark_client::Blockchain for SimpleBlockchain {
    async fn find_outpoints(
        &self,
        _address: &Address,
    ) -> Result<Vec<ark_client::ExplorerUtxo>, ark_client::Error> {
        Ok(vec![])
    }

    async fn find_tx(
        &self,
        _txid: &bitcoin::Txid,
    ) -> Result<Option<bitcoin::Transaction>, ark_client::Error> {
        Ok(None)
    }

    async fn get_output_status(
        &self,
        _txid: &bitcoin::Txid,
        _vout: u32,
    ) -> Result<ark_client::SpendStatus, ark_client::Error> {
        Ok(ark_client::SpendStatus { spend_txid: None })
    }

    async fn broadcast(&self, _tx: &bitcoin::Transaction) -> Result<(), ark_client::Error> {
        Ok(())
    }

    async fn get_fee_rate(&self) -> Result<f64, ark_client::Error> {
        Ok(1.0)
    }

    async fn broadcast_package(
        &self,
        _txs: &[&bitcoin::Transaction],
    ) -> Result<(), ark_client::Error> {
        Ok(())
    }
}

struct DummyWallet;

impl ark_client::wallet::OnchainWallet for DummyWallet {
    fn get_onchain_address(&self) -> Result<Address, ark_client::Error> {
        Ok("bc1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqp3whfpx"
            .parse()
            .unwrap())
    }

    async fn sync(&self) -> Result<(), ark_client::Error> {
        Ok(())
    }

    fn balance(&self) -> Result<ark_client::wallet::Balance, ark_client::Error> {
        Ok(ark_client::wallet::Balance::default())
    }

    fn prepare_send_to_address(
        &self,
        _address: Address,
        _amount: bitcoin::Amount,
        _fee_rate: bitcoin::FeeRate,
    ) -> Result<bitcoin::psbt::PartiallySignedTransaction, ark_client::Error> {
        Err(ark_client::Error::ad_hoc("not implemented"))
    }

    fn sign(
        &self,
        _psbt: &mut bitcoin::psbt::PartiallySignedTransaction,
    ) -> Result<bool, ark_client::Error> {
        Ok(false)
    }

    fn select_coins(
        &self,
        _target_amount: bitcoin::Amount,
    ) -> Result<ark_core::UtxoCoinSelection, ark_client::Error> {
        Err(ark_client::Error::ad_hoc("not implemented"))
    }
}

impl ark_client::wallet::BoardingWallet for DummyWallet {
    fn new_boarding_output(
        &self,
        _server_pk: bitcoin::XOnlyPublicKey,
        _exit_delay: bitcoin::Sequence,
        _network: bitcoin::Network,
    ) -> Result<ark_core::BoardingOutput, ark_client::Error> {
        Err(ark_client::Error::ad_hoc("not implemented"))
    }

    fn get_boarding_outputs(&self) -> Result<Vec<ark_core::BoardingOutput>, ark_client::Error> {
        Ok(vec![])
    }

    fn sign_for_pk(
        &self,
        _pk: &bitcoin::XOnlyPublicKey,
        _msg: &bitcoin::secp256k1::Message,
    ) -> Result<bitcoin::secp256k1::schnorr::Signature, ark_client::Error> {
        Err(ark_client::Error::ad_hoc("not implemented"))
    }
}

static mut CLIENT: Option<Client<SimpleBlockchain, DummyWallet>> = None;

#[no_mangle]
pub extern "C" fn init_client() {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[1u8; 32]).unwrap();
    let keypair = Keypair::from_secret_key(&secp, &secret_key);

    let blockchain = Arc::new(SimpleBlockchain);
    let wallet = Arc::new(DummyWallet);

    let offline = OfflineClient::new(
        "flutter".to_string(),
        keypair,
        blockchain,
        wallet,
        "https://mutinynet.arkade.sh".to_string(),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = rt.block_on(async move { offline.connect().await.unwrap() });

    unsafe {
        CLIENT = Some(client);
    }
}

#[no_mangle]
pub extern "C" fn get_offchain_address() -> *const c_char {
    let client = unsafe { CLIENT.as_ref().unwrap() };
    let (addr, _) = client.get_offchain_address().unwrap();
    let s = addr.to_string();
    CString::new(s).unwrap().into_raw()
}
