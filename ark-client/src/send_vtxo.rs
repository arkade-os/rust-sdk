use crate::error::ErrorContext;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::coin_select::select_vtxos;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::ArkAddress;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    /// Spend confirmed and pre-confimed VTXOs in an Ark transaction sending the given `amount` to
    /// the given `address`.
    ///
    /// The Ark transaction is built in collaboration with the Ark server. The outputs of said
    /// transaction will be pre-confirmed VTXOs.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn send_vtxo(&self, address: ArkAddress, amount: Amount) -> Result<Txid, Error> {
        // Recoverable VTXOs cannot be sent.
        let select_recoverable_vtxos = false;

        let spendable_vtxos = self
            .spendable_vtxos(select_recoverable_vtxos)
            .await
            .context("failed to get spendable VTXOs")?;

        // Run coin selection algorithm on candidate spendable VTXOs.
        let spendable_virtual_tx_outpoints = spendable_vtxos
            .iter()
            .flat_map(|(vtxos, _)| vtxos.clone())
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
            })
            .collect::<Vec<_>>();

        let selected_coins = select_vtxos(
            spendable_virtual_tx_outpoints,
            amount,
            self.server_info.dust,
            true,
        )
        .map_err(Error::from)
        .context("failed to select coins")?;

        let vtxo_inputs = selected_coins
            .into_iter()
            .map(|virtual_tx_outpoint| {
                let vtxo = spendable_vtxos
                    .clone()
                    .into_iter()
                    .find_map(|(virtual_tx_outpoints, vtxo)| {
                        virtual_tx_outpoints
                            .iter()
                            .any(|v| v.outpoint == virtual_tx_outpoint.outpoint)
                            .then_some(vtxo)
                    })
                    .expect("to find matching default VTXO");

                send::VtxoInput::new(
                    vtxo,
                    virtual_tx_outpoint.amount,
                    virtual_tx_outpoint.outpoint,
                )
            })
            .collect::<Vec<_>>();

        let (change_address, _) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&address, amount)],
            Some(&change_address),
            &vtxo_inputs,
            self.server_info.dust,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        let sign_fn =
        |msg: secp256k1::Message| -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
            let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, self.kp());
            let pk = self.kp().x_only_public_key().0;

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

        let mut res = self
            .network_client()
            .submit_offchain_transaction_request(
                ark_tx,
                checkpoint_txs
                    .into_iter()
                    .map(|(psbt, _, _, _)| psbt)
                    .collect(),
            )
            .await
            .map_err(Error::ark_server)
            .context("failed to submit offchain transaction request")?;

        for checkpoint_psbt in res.signed_checkpoint_txs.iter_mut() {
            let vtxo_input = vtxo_inputs
                .iter()
                .find(|input| {
                    checkpoint_psbt.unsigned_tx.input[0].previous_output == input.outpoint()
                })
                .ok_or_else(|| {
                    Error::ad_hoc(format!(
                        "could not find VTXO input for checkpoint transaction {}",
                        checkpoint_psbt.unsigned_tx.compute_txid(),
                    ))
                })?;

            sign_checkpoint_transaction(sign_fn, checkpoint_psbt, vtxo_input)?;
        }

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, res.signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        Ok(ark_txid)
    }
}
