use crate::coin_select::coin_select_for_onchain;
use crate::error::Error;
use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::AnchorSpendDeps;
use crate::Blockchain;
use crate::Client;
use ark_core::build_unilateral_exit_tree_txids;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::unilateral_exit;
use ark_core::unilateral_exit::create_unilateral_exit_transaction;
use ark_core::unilateral_exit::finalize_unilateral_exit_tree;
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
impl<B, S> Client<B, S>
where
    B: Blockchain,
    S: SwapStorage + 'static,
{
    /// Build the unilateral exit transaction tree for all spendable VTXOs.
    ///
    /// ### Returns
    ///
    /// The tree as a `Vec<Vec<Transaction>>`, where each branch represents a path from a
    /// commitment transaction output to a spendable VTXO. Every transaction is finalized, but
    /// requires fee bumping through a P2A output.
    pub async fn build_unilateral_exit_trees(&self) -> Result<Vec<Vec<Transaction>>, Error> {
        let vtxo_list = self
            .list_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

        let mut unilateral_exit_trees = Vec::new();

        // For each spendable VTXO, generate its unilateral exit tree.
        for contract_vtxo in vtxo_list.could_exit_unilaterally() {
            let virtual_tx_outpoint = contract_vtxo.vtxo();
            let vtxo_chain_response = timeout_op(
                self.inner.timeout,
                self.network_client().get_vtxo_chain(
                    Some(virtual_tx_outpoint.outpoint),
                    None,
                    None,
                    None,
                ),
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
            let txs = HashSet::<Txid>::from_iter(paths.concat());

            let virtual_txs_response = timeout_op(
                self.inner.timeout,
                self.network_client().get_virtual_txs(
                    txs.iter().map(|tx| tx.to_string()).collect(),
                    None,
                    None,
                ),
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

            let finalized_unilateral_exit_tree =
                finalize_unilateral_exit_tree(&unilateral_exit_tree, commitment_txs.as_slice())?;
            branches.extend(finalized_unilateral_exit_tree);
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
        deps: &AnchorSpendDeps<'_>,
    ) -> Result<Option<Txid>, Error> {
        let blockchain = &self.blockchain();

        for parent_tx in branch {
            let parent_txid = parent_tx.compute_txid();

            let broadcast = || async {
                let is_not_published = blockchain.find_tx(&parent_txid).await?.is_none();

                if is_not_published {
                    let child_tx = self.bump_tx(parent_tx, deps).await?;
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
    ///
    /// `change_address` MUST be spendable by the caller. Since all boarding outputs and VTXOs
    /// are spent, the change can be nearly the entire exited balance, and funds sent to an address
    /// the caller does not control are lost irrecoverably. Only the address network is validated
    /// here.
    pub async fn send_on_chain(
        &self,
        to_address: Address,
        to_amount: Amount,
        change_address: Address,
    ) -> Result<Txid, Error> {
        let (tx, _) = self
            .create_send_on_chain_transaction_inner(to_address, to_amount, change_address)
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

    /// Build the on-chain send transaction without broadcasting.
    ///
    /// Primarily useful for testing. Exposed publicly behind the `test-utils` feature.
    ///
    /// `change_address` MUST be spendable by the caller.
    #[cfg(feature = "test-utils")]
    pub async fn create_send_on_chain_transaction(
        &self,
        to_address: Address,
        to_amount: Amount,
        change_address: Address,
    ) -> Result<(Transaction, Vec<TxOut>), Error> {
        self.create_send_on_chain_transaction_inner(to_address, to_amount, change_address)
            .await
    }

    /// `change_address` MUST be spendable by the caller.
    pub(crate) async fn create_send_on_chain_transaction_inner(
        &self,
        to_address: Address,
        to_amount: Amount,
        change_address: Address,
    ) -> Result<(Transaction, Vec<TxOut>), Error> {
        let server_info = self.server_info().await?;
        let network = server_info.network;

        for (label, address) in [("destination", &to_address), ("change", &change_address)] {
            if !address.as_unchecked().is_valid_for_network(network) {
                return Err(Error::ad_hoc(format!(
                    "invalid {label} address {address}: not valid for network {network}"
                )));
            }
        }

        let dust = server_info.dust;
        if to_amount < dust {
            return Err(Error::ad_hoc(format!(
                "invalid amount {to_amount}, must be greater than dust: {}",
                dust,
            )));
        }

        // TODO: Do not use an arbitrary fee.
        let fee = Amount::from_sat(1_000);

        let (onchain_inputs, vtxo_inputs) = coin_select_for_onchain(self, to_amount + fee).await?;

        let sign = move |input: &mut psbt::Input, msg: bitcoin::secp256k1::Message| match &input
            .witness_script
        {
            None => Err(ark_core::Error::ad_hoc(
                "Missing witness script for psbt::Input when signing unilateral exit transaction",
            )),
            Some(script) => {
                let mut res = vec![];
                let pks = extract_checksig_pubkeys(script);

                for pk in pks {
                    if let Ok(keypair) = self.keypair_by_pk(&pk) {
                        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                        let pk = keypair.x_only_public_key().0;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ContractManager;
    use crate::swap_storage::InMemorySwapStorage;
    use crate::ExplorerUtxo;
    use crate::OfflineClient;
    use crate::OfflineClientConfig;
    use crate::ServerState;
    use crate::SpendStatus;
    use crate::TxStatus;
    use ark_core::anchor_output;
    use ark_core::server::Info;
    use ark_core::SelectedUtxo;
    use ark_core::UtxoCoinSelection;
    use bitcoin::key::Keypair;
    use bitcoin::secp256k1;
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::Network;
    use bitcoin::Psbt;
    use bitcoin::ScriptBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::RwLock;
    use std::time::Duration;
    use std::time::Instant;

    const DUST: Amount = Amount::from_sat(1_000);

    #[derive(Clone)]
    struct DummyBlockchain;

    impl Blockchain for DummyBlockchain {
        async fn find_outpoints(&self, _address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
            Ok(Vec::new())
        }

        async fn find_tx(&self, _txid: &Txid) -> Result<Option<Transaction>, Error> {
            Ok(None)
        }

        async fn get_tx_status(&self, _txid: &Txid) -> Result<TxStatus, Error> {
            Ok(TxStatus { confirmed_at: None })
        }

        async fn get_output_status(&self, _txid: &Txid, _vout: u32) -> Result<SpendStatus, Error> {
            Ok(SpendStatus { spend_txid: None })
        }

        async fn broadcast(&self, _tx: &Transaction) -> Result<(), Error> {
            Ok(())
        }

        async fn get_fee_rate(&self) -> Result<f64, Error> {
            Ok(1.0)
        }

        async fn broadcast_package(&self, _txs: &[&Transaction]) -> Result<(), Error> {
            Ok(())
        }
    }

    fn p2tr_address(network: Network, byte: u8) -> Address {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[byte; 32]).unwrap());

        Address::p2tr(&secp, keypair.x_only_public_key().0, None, network)
    }

    fn test_server_info(network: Network) -> Info {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
        let public_key = secp256k1::PublicKey::from_secret_key(&secp, &secret_key);

        Info {
            version: "test".to_string(),
            signer_pk: public_key,
            forfeit_pk: public_key,
            forfeit_address: p2tr_address(network, 1),
            checkpoint_tapscript: ScriptBuf::new(),
            network,
            session_duration: 60,
            unilateral_exit_delay: bitcoin::Sequence::from_height(144),
            boarding_exit_delay: bitcoin::Sequence::from_height(144),
            utxo_min_amount: None,
            utxo_max_amount: None,
            vtxo_min_amount: None,
            vtxo_max_amount: None,
            dust: DUST,
            fees: None,
            scheduled_session: None,
            deprecated_signers: Vec::new(),
            service_status: Default::default(),
            digest: "digest".to_string(),
            max_tx_weight: 0,
            max_op_return_outputs: 0,
        }
    }

    fn test_client(network: Network) -> Client<DummyBlockchain, InMemorySwapStorage> {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
        let inner = OfflineClient::<DummyBlockchain, InMemorySwapStorage>::with_keypair(
            OfflineClientConfig {
                // No test in this module is allowed to reach the network: every assertion must be
                // made before the client would call out to the Ark server.
                ark_server_url: "http://127.0.0.1:1".to_string(),
                boltz_url: "http://127.0.0.1:1".to_string(),
                timeout: Duration::from_millis(50),
                ..Default::default()
            },
            keypair,
            Arc::new(DummyBlockchain),
            Arc::new(InMemorySwapStorage::default()),
        );

        let server_info = test_server_info(network);
        let mut contract_manager = ContractManager::in_memory(server_info.network);
        contract_manager.register_builtins().unwrap();

        Client {
            inner,
            state: Arc::new(RwLock::new(ServerState {
                fee_estimator: ark_fees::Estimator::new(Default::default()).unwrap(),
                server_info,
                server_info_refreshed_at: Instant::now(),
                contract_manager: Mutex::new(contract_manager),
            })),
            server_info_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    #[tokio::test]
    async fn send_on_chain_rejects_wrong_network_change_address() {
        let client = test_client(Network::Regtest);

        let error = client
            .create_send_on_chain_transaction_inner(
                p2tr_address(Network::Regtest, 4),
                DUST * 2,
                p2tr_address(Network::Bitcoin, 5),
            )
            .await
            .expect_err("mainnet change address must be rejected on a regtest server");

        assert!(
            error.to_string().contains("invalid change address"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn send_on_chain_rejects_wrong_network_destination_address() {
        let client = test_client(Network::Regtest);

        let error = client
            .create_send_on_chain_transaction_inner(
                p2tr_address(Network::Bitcoin, 4),
                DUST * 2,
                p2tr_address(Network::Regtest, 5),
            )
            .await
            .expect_err("mainnet destination address must be rejected on a regtest server");

        assert!(
            error.to_string().contains("invalid destination address"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn send_on_chain_accepts_addresses_on_the_server_network() {
        let client = test_client(Network::Regtest);

        let error = client
            .create_send_on_chain_transaction_inner(
                p2tr_address(Network::Regtest, 4),
                DUST - Amount::ONE_SAT,
                p2tr_address(Network::Regtest, 5),
            )
            .await
            .expect_err("an amount below dust must be rejected");

        assert!(
            error.to_string().contains("must be greater than dust"),
            "unexpected error: {error}"
        );
    }

    fn tx_with_anchor_output() -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::non_standard(3),
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: vec![anchor_output()],
        }
    }

    fn funded_coin_selection() -> UtxoCoinSelection {
        UtxoCoinSelection {
            selected_utxos: vec![SelectedUtxo {
                outpoint: bitcoin::OutPoint {
                    txid: Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros()),
                    vout: 0,
                },
                amount: Amount::from_sat(100_000),
                address: p2tr_address(Network::Regtest, 6),
            }],
            total_selected: Amount::from_sat(100_000),
            change_amount: Amount::from_sat(50_000),
        }
    }

    fn deps_with(
        change_address: impl Fn() -> Result<Address, Error> + Send + Sync + 'static,
        select_coins: impl Fn(Amount) -> Result<UtxoCoinSelection, Error> + Send + Sync + 'static,
        sign: impl Fn(&mut Psbt) -> Result<bool, Error> + Send + Sync + 'static,
    ) -> AnchorSpendDeps<'static> {
        AnchorSpendDeps {
            change_address: Box::new(change_address),
            select_coins: Box::new(select_coins),
            sign: Box::new(sign),
        }
    }

    #[tokio::test]
    async fn bump_tx_propagates_change_address_error() {
        let client = test_client(Network::Regtest);
        let deps = deps_with(
            || Err(Error::wallet("no change address")),
            |_| Ok(funded_coin_selection()),
            |_| Ok(true),
        );

        let error = client
            .bump_tx(&tx_with_anchor_output(), &deps)
            .await
            .expect_err("a failing change address closure must fail the bump");

        assert!(
            error.to_string().contains("no change address"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn bump_tx_propagates_select_coins_error() {
        let client = test_client(Network::Regtest);
        let deps = deps_with(
            || Ok(p2tr_address(Network::Regtest, 5)),
            |_| Err(Error::wallet("insufficient funds")),
            |_| Ok(true),
        );

        let error = client
            .bump_tx(&tx_with_anchor_output(), &deps)
            .await
            .expect_err("a failing coin selection closure must fail the bump");

        assert!(
            error.to_string().contains("insufficient funds"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn bump_tx_propagates_sign_error() {
        let client = test_client(Network::Regtest);
        let deps = deps_with(
            || Ok(p2tr_address(Network::Regtest, 5)),
            |_| Ok(funded_coin_selection()),
            |_| Err(Error::wallet("cannot sign")),
        );

        let error = client
            .bump_tx(&tx_with_anchor_output(), &deps)
            .await
            .expect_err("a failing signing closure must fail the bump");

        assert!(
            error.to_string().contains("cannot sign"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn bump_tx_rejects_psbt_that_sign_could_not_finalize() {
        let client = test_client(Network::Regtest);
        let deps = deps_with(
            || Ok(p2tr_address(Network::Regtest, 5)),
            |_| Ok(funded_coin_selection()),
            |_| Ok(false),
        );

        let error = client
            .bump_tx(&tx_with_anchor_output(), &deps)
            .await
            .expect_err("an unfinalized PSBT must not be extracted");

        assert!(
            error.to_string().contains("was not finalized"),
            "unexpected error: {error}"
        );
    }
}
