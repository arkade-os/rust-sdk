use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::PublicKey;
use bitcoin::XOnlyPublicKey;
use rand::thread_rng;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;

const BOLTZ_URL: &str = "https://api.boltz.mutinynet.arkade.sh/v2";

pub struct CreateLightningInvoiceResponse {
    amount: Amount,
    expiry: i64,
    invoice: String,
    payment_hash: String,
    pending_reverse_swap: PendingReverseSwap,
    preimage: String,
}

pub struct PendingReverseSwap {
    created_at: i64,
    preimage: String,
    status: SwapStatus,
    claim_public_key: XOnlyPublicKey,
    response: ReverseSwapResponse,
}

pub enum SwapStatus {
    InvoiceExpired,
    InvoiceFailedToPay,
    InvoicePaid,
    InvoicePending,
    InvoiceSet,
    InvoiceSettled,
    SwapCreated,
    SwapExpired,
    TransactionClaimPending,
    TransactionClaimed,
    TransactionConfirmed,
    TransactionFailed,
    TransactionLockupFailed,
    TransactionMempool,
    TransactionRefunded,
}

// receive from lightning = reverse submarine swap
//
// 1. create invoice by creating a reverse swap
// 2. monitor incoming payment by waiting for the hold invoice to be paid
// 3. claim the VHTLC by creating a virtual transaction that spends the VHTLC output
// 4. return the preimage and the swap info
impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    /// Settle _all_ prior VTXOs and boarding outputs into the next batch, generating new confirmed
    /// VTXOs.
    pub async fn create_ln_invoice(
        &self,
        amount: Amount,
        description: String,
    ) -> Result<CreateLightningInvoiceResponse, Error> {
        if amount == Amount::ZERO {
            return Err(Error::ad_hoc("cannot create LN invoice for 0 sats"));
        }

        let (claim_public_key, _) = self.kp().x_only_public_key();

        let mut rng = thread_rng();
        let mut preimage = vec![0u8; 32];

        rng.fill_bytes(&mut preimage);

        let hash_preimage = sha256::Hash::hash(&preimage);
        let hash_preimage = hash_preimage.to_byte_array().to_lower_hex_string();

        dbg!(format!("{}", claim_public_key));

        let request = ReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            claim_public_key,
            invoice_amount: amount,
            preimage_hash: hash_preimage,
        };

        let res = reqwest::Client::new()
            .post(format!("{BOLTZ_URL}/swap/reverse"))
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed calling /swap/reverse Boltz API: {e:?}")))?;

        dbg!(res.text().await);

        // let res = match res.error_for_status() {
        //     Err(e) => {
        //         return Err(Error::ad_hoc(format!(
        //             "Got {:?} from /swap/reverse Boltz API: {e:?}",
        //             e.status(),
        //         )));
        //     }
        //     res => res,
        // };

        // let res = res
        //     .map_err(|e| Error::ad_hoc(format!("Got error from /swap/reverse Boltz API:
        // {e:?}")))?;

        // let response: ReverseSwapResponse = res.json().await.map_err(|e| {
        //     Error::ad_hoc(format!("Failed to deserialize ReverseSwapResponse: {e:?}"))
        // })?;

        // dbg!(&response);

        Ok(CreateLightningInvoiceResponse {
            amount,
            expiry: todo!(),
            invoice: todo!(),
            payment_hash: todo!(),
            pending_reverse_swap: todo!(),
            preimage: todo!(),
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReverseSwapRequest {
    from: Asset,
    to: Asset,
    claim_public_key: XOnlyPublicKey,
    invoice_amount: Amount,
    preimage_hash: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReverseSwapResponse {
    id: String,
    invoice: String,
    onchain_amount: Amount,
    lockup_address: Address<NetworkUnchecked>,
    refund_pk: PublicKey,
    timeout_block_heights: TimeoutBlockHeights,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutBlockHeights {
    refund: u64,
    unilateral_claim: u64,
    unilateral_refund: u64,
    unilateral_refund_without_receiver: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Asset {
    Btc,
    Ark,
}
