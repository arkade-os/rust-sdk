use crate::coin_select::coin_select_for_onchain;
use crate::error::Error;
use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::OnchainWallet;
use crate::wallet::Persistence;
use crate::Blockchain;
use crate::Client;
use ark_core::build_unilateral_exit_tree_txids;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::unilateral_exit;
use ark_core::unilateral_exit::create_unilateral_exit_transaction;
use ark_core::unilateral_exit::sign_unilateral_exit_tree;
use ark_core::unilateral_exit::UnilateralExitTree;
use backon::ExponentialBuilder;
use backon::Retryable;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::Txid;
use std::collections::HashSet;

// TODO: We should not _need_ to connect to the Ark server to perform unilateral exit. Currently we
// do talk to the Ark server for simplicity.
impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: OnchainWallet + Persistence,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    /// Build the unilateral exit transaction tree for all spendable VTXOs.
    ///
    /// ### Returns
    ///
    /// The tree as a `Vec<Vec<Transaction>>`, where each branch represents a path from
    /// commitment transaction output to a spendable VTXO. Every transaction is fully signed,
    /// but requires fee bumping through a P2A output.
    pub async fn build_unilateral_exit_trees(&self) -> Result<Vec<Vec<Transaction>>, Error> {
        let (vtxo_list, _) = self
            .list_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

        let mut unilateral_exit_trees = Vec::new();

        // For each spendable VTXO, generate its unilateral exit tree.
        for virtual_tx_outpoint in vtxo_list.could_exit_unilaterally() {
            let vtxo_chain_response = timeout_op(
                self.inner.timeout,
                self.network_client()
                    .get_vtxo_chain(Some(virtual_tx_outpoint.outpoint), None),
            )
            .await
            .context(format!(
                "failed to get VTXO chain for outpoint {}",
                virtual_tx_outpoint.outpoint
            ))??;

            let paths = build_unilateral_exit_tree_txids(
                &vtxo_chain_response.chains,
                virtual_tx_outpoint.outpoint.txid,
            )?;

            // We don't want to fetch transactions more than once.
            let txs = HashSet::<Txid>::from_iter(paths.concat().into_iter());

            let virtual_txs_response = timeout_op(
                self.inner.timeout,
                self.network_client()
                    .get_virtual_txs(txs.iter().map(|tx| tx.to_string()).collect(), None),
            )
            .await
            .context("failed to get virtual TXs")??;

            let paths = paths
                .into_iter()
                .map(|path| {
                    path.into_iter()
                        .map(|txid| {
                            virtual_txs_response
                                .txs
                                .iter()
                                .find(|t| t.unsigned_tx.compute_txid() == txid)
                                .cloned()
                                .ok_or_else(|| {
                                    Error::ad_hoc(format!("no PSBT found for virtual TX {txid}"))
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;

            let unilateral_exit_tree =
                UnilateralExitTree::new(virtual_tx_outpoint.commitment_txids.clone(), paths);

            unilateral_exit_trees.push(unilateral_exit_tree);
        }

        let mut branches: Vec<Vec<Transaction>> = Vec::new();
        for unilateral_exit_tree in unilateral_exit_trees {
            let commitment_txids = unilateral_exit_tree.commitment_txids();

            let mut commitment_txs = Vec::new();
            for commitment_txid in commitment_txids.iter() {
                let commitment_tx = timeout_op(
                    self.inner.timeout,
                    self.blockchain().find_tx(commitment_txid),
                )
                .await??
                .ok_or_else(|| {
                    Error::ad_hoc(format!("could not find commitment TX {commitment_txid}"))
                })?;

                commitment_txs.push(commitment_tx);
            }

            let signed_unilateral_exit_tree =
                sign_unilateral_exit_tree(&unilateral_exit_tree, commitment_txs.as_slice())?;
            branches.extend(signed_unilateral_exit_tree);
        }

        Ok(branches)
    }

    /// Broadcast the next unconfirmed transaction in a branch, skipping transactions that are
    /// already on the blockchain.
    ///
    /// ### Returns
    ///
    /// `Ok(Some(txid))` if a transaction was broadcast, `Ok(None)` if all are confirmed.
    pub async fn broadcast_next_unilateral_exit_node(
        &self,
        branch: &[Transaction],
    ) -> Result<Option<Txid>, Error> {
        let blockchain = &self.blockchain();

        for parent_tx in branch {
            let parent_txid = parent_tx.compute_txid();

            let broadcast = || async {
                let is_not_published = blockchain.find_tx(&parent_txid).await?.is_none();

                if is_not_published {
                    let child_tx = self.bump_tx(parent_tx).await?;
                    let bump_txid = child_tx.compute_txid();

                    tracing::info!(
                        txid = %parent_txid,
                        %bump_txid,
                        "Broadcasting unilateral exit TX"
                    );

                    blockchain
                        .broadcast_package(&[parent_tx, &child_tx])
                        .await?;

                    Ok(Some(parent_txid))
                } else {
                    tracing::debug!(
                        %parent_txid,
                        "Unilateral exit TX already found on the blockchain"
                    );

                    Ok(None)
                }
            };

            let res = broadcast
                .retry(ExponentialBuilder::default().with_max_times(5))
                .sleep(sleep)
                .notify(|err: &Error, dur: std::time::Duration| {
                    tracing::warn!(
                        "Retrying broadcasting VTXO transaction {parent_txid} after {dur:?}. Error: {err}",
                    );
                })
                .await
                .with_context(|| format!("Failed to broadcast VTXO transaction {parent_txid}"))?;

            if let Some(bump_txid) = res {
                tracing::info!(
                    txid = %parent_txid,
                    %bump_txid,
                    "Broadcast VTXO transaction"
                );

                return Ok(Some(parent_txid));
            }
        }

        // All transactions in the branch are already on-chain
        Ok(None)
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

        timeout_op(self.inner.timeout, self.blockchain().broadcast(&tx))
            .await
            .with_context(|| format!("failed to broadcast transaction {txid}"))??;

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

        let sign = move |input: &mut psbt::Input, msg: bitcoin::secp256k1::Message| match &input
            .witness_script
        {
            None => Err(ark_core::Error::ad_hoc(
                "Missing witness script for psbt::Input when signing unilateral exit transaction",
            )),
            Some(script) => {
                let mut res = vec![];
                let pks = extract_checksig_pubkeys(script);
                let secp = self.secp();

                for pk in pks {
                    if let Ok(keypair) = self.keypair_by_pk(&pk) {
                        let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                        let pk = keypair.x_only_public_key().0;
                        res.push((sig, pk))
                    }

                    if let Ok(sk) = self.inner.wallet.sk_for_pk(&pk) {
                        let sig = secp.sign_schnorr_no_aux_rand(&msg, &sk.keypair(&secp));
                        res.push((sig, pk))
                    }
                }

                Ok(res)
            }
        };

        let tx = create_unilateral_exit_transaction(
            to_address,
            to_amount,
            change_address,
            &onchain_inputs,
            &vtxo_inputs,
            sign,
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
