use crate::batch;
use crate::batch::BatchOutputType;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Client;
use crate::Error;
use crate::KeyProvider;
use crate::SwapStorage;
use ark_core::ArkAddress;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::SignedAmount;
use rand::CryptoRng;
use rand::Rng;

impl<B, W, S, K> Client<B, W, S, K>
where
    B: crate::Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: KeyProvider,
{
    /// Estimates the fee to collaboratively redeem VTXOs to an on-chain Bitcoin address.
    ///
    /// This function calculates the expected fee for moving funds from the Ark protocol
    /// back to a standard on-chain Bitcoin address through a collaborative redemption process.
    /// The fee is estimated by creating a simulated intent and querying the Ark server.
    ///
    /// # Arguments
    ///
    /// * `rng` - A random number generator for creating the intent
    /// * `to_address` - The on-chain Bitcoin address to send funds to
    /// * `to_amount` - The amount to send to the destination address
    ///
    /// # Returns
    ///
    /// Returns the estimated fee as a [`SignedAmount`]. The fee will be deducted from
    /// the total available balance when performing the actual redemption.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The available balance is insufficient for the requested amount
    /// - Failed to fetch VTXOs or boarding inputs
    /// - Failed to communicate with the Ark server
    pub async fn estimate_onchain_fees<R>(
        &self,
        rng: &mut R,
        to_address: Address,
        to_amount: Amount,
    ) -> Result<SignedAmount, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let (change_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) =
            self.fetch_commitment_transaction_inputs().await?;

        let change_amount = total_amount.checked_sub(to_amount).ok_or_else(|| {
            Error::coin_select("cannot afford to send {to_amount}, only have {total_amount}")
        })?;

        tracing::info!(
            %to_address,
            gross_amount = %to_amount,
            change_address = %change_address.encode(),
            %change_amount,
            ?boarding_inputs,
            "Estimating fee to collaboratively redeem outputs"
        );

        let intent = self.prepare_intent(
            &mut rng.clone(),
            boarding_inputs,
            vtxo_inputs,
            BatchOutputType::OffBoard {
                to_address,
                to_amount,
                change_address,
                change_amount,
            },
            batch::IntentMessageType::EstimateIntentFee,
        )?;

        let amount = self.network_client().estimate_fees(intent.intent).await?;

        Ok(amount)
    }

    /// Estimates the fee to join the next batch and settle funds to an Ark address.
    ///
    /// This function calculates the expected fee for consolidating all available VTXOs
    /// and boarding outputs into fresh VTXOs through the Ark batch process. The full
    /// available balance will be used, with fees deducted from the resulting VTXO.
    ///
    /// Use this to estimate fees before calling [`settle`](crate::Client::settle) or
    /// similar batch operations.
    ///
    /// # Arguments
    ///
    /// * `rng` - A random number generator for creating the intent
    /// * `to_address` - The Ark address to receive the settled funds
    ///
    /// # Returns
    ///
    /// Returns the estimated fee as a [`SignedAmount`]. This fee will be deducted from
    /// the total available balance when joining the actual batch.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to fetch VTXOs or boarding inputs
    /// - Failed to communicate with the Ark server
    pub async fn estimate_batch_fees<R>(
        &self,
        rng: &mut R,
        to_address: ArkAddress,
    ) -> Result<SignedAmount, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let (boarding_inputs, vtxo_inputs, total_amount) =
            self.fetch_commitment_transaction_inputs().await?;

        tracing::info!(
            %to_address,
            gross_amount = %total_amount,
            ?boarding_inputs,
            "Estimating fee to board outputs"
        );

        let intent = self.prepare_intent(
            &mut rng.clone(),
            boarding_inputs,
            vtxo_inputs,
            BatchOutputType::Board {
                to_address,
                to_amount: total_amount,
            },
            batch::IntentMessageType::EstimateIntentFee,
        )?;

        let amount = self.network_client().estimate_fees(intent.intent).await?;

        Ok(amount)
    }
}
