use crate::coin_select::coin_select_for_onchain;
use crate::error::Error;
use crate::error::ErrorContext;
use crate::utils::sleep;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use ark_core::unilateral_exit;
use ark_core::unilateral_exit::create_unilateral_exit_transaction;
use ark_core::unilateral_exit::prepare_vtxo_tree_transactions;
use backon::ExponentialBuilder;
use backon::Retryable;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::Txid;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;

// TODO: We should not _need_ to connect to the Ark server to perform unilateral exit. Currently we
// do talk to the Ark server for simplicity.
impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    /// Publish all the relevant transactions in the VTXO tree to get our VTXOs on chain.
    pub async fn commit_vtxos_on_chain(&self) -> Result<(), Error> {
        let spendable_vtxos = self.spendable_vtxos().await?;

        let network_client = &self.network_client();
        let vtxos = spendable_vtxos
            .into_iter()
            .flat_map(|(vtxo_outpoints, _)| {
                vtxo_outpoints
                    .into_iter()
                    .map(|vtxo_outpoint| match &vtxo_outpoint.redeem_txs {
                        redeem_txs if !redeem_txs.is_empty() => {
                            unilateral_exit::VtxoProvenance::new_unconfirmed(
                                vtxo_outpoint.outpoint,
                                vtxo_outpoint.round_txid,
                                redeem_txs.clone(),
                            )
                        }
                        _ => unilateral_exit::VtxoProvenance::new(
                            vtxo_outpoint.outpoint,
                            vtxo_outpoint.round_txid,
                        ),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut rounds = HashMap::new();
        for vtxo in vtxos.iter() {
            let round_txid = vtxo.round_txid();
            if let Entry::Vacant(e) = rounds.entry(round_txid) {
                let round = network_client
                    .get_round(round_txid.to_string())
                    .await
                    .map_err(Error::ark_server)?
                    .ok_or_else(|| Error::ad_hoc(format!("could not find round {round_txid}")))?;

                e.insert(round);
            }
        }

        let off_board_txs =
            prepare_vtxo_tree_transactions(vtxos.as_slice(), rounds).map_err(Error::from)?;

        let blockchain = &self.blockchain();

        // Sort transactions by dependency order - ancestors must be broadcast before descendants
        let mut sorted_txs = Vec::new();
        let mut processed_txids = HashSet::new();
        
        // Helper function to recursively add transactions in dependency order
        fn add_tx_with_deps(
            tx: &Transaction,
            all_txs: &[Transaction],
            sorted_txs: &mut Vec<Transaction>,
            processed_txids: &mut HashSet<Txid>,
        ) {
            // Skip if already processed
            if processed_txids.contains(&tx.compute_txid()) {
                return;
            }

            // First add all parent transactions
            for input in tx.input.iter() {
                if let Some(parent_tx) = all_txs.iter().find(|t| t.compute_txid() == input.previous_output.txid) {
                    add_tx_with_deps(parent_tx, all_txs, sorted_txs, processed_txids);
                }
            }

            // Then add this transaction
            sorted_txs.push(tx.clone());
            processed_txids.insert(tx.compute_txid());
        }

        // Sort all transactions by dependency order
        for tx in off_board_txs.iter() {
            add_tx_with_deps(tx, &off_board_txs, &mut sorted_txs, &mut processed_txids);
        }

        // Broadcast transactions in order
        for tx in sorted_txs.iter() {
            let txid = tx.compute_txid();

            let is_not_published = blockchain.find_tx(&txid).await?.is_none();
            if is_not_published {
                tracing::info!(%txid, "Broadcasting VTXO transaction");
                let broadcast = || async { blockchain.broadcast(tx).await };

                broadcast
                    .retry(ExponentialBuilder::default().with_max_times(5))
                    .sleep(sleep)
                    .notify(|err: &Error, dur: std::time::Duration| {
                        tracing::warn!(
                            "Retrying broadcasting VTXO transaction {txid} after {dur:?}. Error: {err}",
                        );
                    })
                    .await
                    .with_context(|| format!("Failed to broadcast VTXO transaction {txid}"))?;

                tracing::info!(%txid, "Broadcasted VTXO transaction");
            }
        }

        Ok(())
    }

    /// Spend boarding outputs and VTXOs to an _on-chain_ address.
    ///
    /// All these outputs are spent unilaterally.
    ///
    /// To be able to spend a boarding output, we must wait for the exit delay to pass.
    ///
    /// To be able to spend a VTXO, the VTXO itself must be published on-chain (via something like
    /// `unilateral_off_board`), and then we must wait for the exit delay to pass.
    pub async fn send_on_chain(
        &self,
        to_address: Address,
        to_amount: Amount,
    ) -> Result<Txid, Error> {
        let (tx, _) = self
            .create_send_on_chain_transaction(to_address, to_amount)
            .await?;

        let txid = tx.compute_txid();
        tracing::info!(
            %txid,
            "Broadcasting transaction sending Ark outputs onchain"
        );

        self.blockchain()
            .broadcast(&tx)
            .await
            .context("failed to broadcast transaction {tx}")?;

        Ok(txid)
    }

    /// Helper function to `send_on_chain`.
    ///
    /// We extract this and keep it as part of the public API to be able to test the resulting
    /// transaction in the e2e tests without needing to wait for a long time.
    ///
    /// TODO: Obviously, it's bad to have this as part of the public API. Do something about it!
    pub async fn create_send_on_chain_transaction(
        &self,
        to_address: Address,
        to_amount: Amount,
    ) -> Result<(Transaction, Vec<TxOut>), Error> {
        if to_amount < self.server_info.dust {
            return Err(Error::ad_hoc(format!(
                "invalid amount {to_amount}, must be greater than dust: {}",
                self.server_info.dust,
            )));
        }

        // TODO: Do not use an arbitrary fee.
        let fee = Amount::from_sat(1_000);

        let (onchain_inputs, vtxo_inputs) = coin_select_for_onchain(self, to_amount + fee).await?;

        let change_address = self.inner.wallet.get_onchain_address()?;

        let tx = create_unilateral_exit_transaction(
            self.kp(),
            to_address,
            to_amount,
            change_address,
            &onchain_inputs,
            &vtxo_inputs,
        )
        .map_err(Error::from)?;

        let prevouts = onchain_inputs
            .iter()
            .map(unilateral_exit::OnChainInput::previous_output)
            .chain(
                vtxo_inputs
                    .iter()
                    .map(unilateral_exit::VtxoInput::previous_output),
            )
            .collect();

        Ok((tx, prevouts))
    }
}
