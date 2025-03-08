use crate::error::ErrorContext;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::coin_select::select_vtxos;
use ark_core::redeem;
use ark_core::redeem::build_redeem_transaction;
use ark_core::redeem::sign_redeem_transaction;
use ark_core::ArkAddress;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::Psbt;
use bitcoin::XOnlyPublicKey;

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    pub async fn send_vtxo(&self, address: ArkAddress, amount: Amount) -> Result<Psbt, Error> {
        let spendable_vtxos = self
            .spendable_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

        // Run coin selection algorithm on candidate spendable VTXOs.
        let spendable_vtxo_outpoints = spendable_vtxos
            .iter()
            .flat_map(|(vtxos, _)| vtxos.clone())
            .map(|vtxo| ark_core::coin_select::VtxoOutPoint {
                outpoint: vtxo.outpoint,
                expire_at: vtxo.expire_at,
                amount: vtxo.amount,
            })
            .collect::<Vec<_>>();

        let selected_coins = select_vtxos(
            spendable_vtxo_outpoints,
            amount,
            self.server_info.dust,
            true,
        )
        .map_err(Error::from)
        .context("failed to select coins")?;

        let vtxo_inputs = selected_coins
            .into_iter()
            .map(|vtxo_outpoint| {
                let vtxo = spendable_vtxos
                    .clone()
                    .into_iter()
                    .find_map(|(vtxo_outpoints, vtxo)| {
                        vtxo_outpoints
                            .iter()
                            .any(|v| v.outpoint == vtxo_outpoint.outpoint)
                            .then_some(vtxo)
                    })
                    .expect("to find matching default VTXO");

                redeem::VtxoInput::new(vtxo, vtxo_outpoint.amount, vtxo_outpoint.outpoint)
            })
            .collect::<Vec<_>>();

        let (change_address, _) = self.get_offchain_address()?;

        let mut redeem_psbt =
            build_redeem_transaction(&[(&address, amount)], Some(&change_address), &vtxo_inputs)
                .map_err(Error::from)?;

        let sign_fn =
        |msg: secp256k1::Message| -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
            let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, self.kp());
            let pk = self.kp().x_only_public_key().0;

            Ok((sig, pk))
        };

        for (i, _) in vtxo_inputs.iter().enumerate() {
            sign_redeem_transaction(sign_fn, &mut redeem_psbt, &vtxo_inputs, i)?;
        }

        self.network_client()
            .submit_redeem_transaction(redeem_psbt.clone())
            .await
            .map_err(Error::ark_server)
            .context("failed to complete payment request")?;

        Ok(redeem_psbt)
    }
}
