use crate::Error;
use crate::server::VirtualTxOutPoint;
use bitcoin::Amount;
use bitcoin::SignedAmount;
use bitcoin::Txid;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transaction {
    /// A transaction that transforms a UTXO into a boarding output.
    Boarding {
        txid: Txid,
        /// We use [`Amount`] because boarding transactions are always incoming i.e. we receive a
        /// boarding output.
        amount: Amount,
        confirmed_at: Option<i64>,
    },
    /// A transaction that confirms VTXOs.
    Commitment {
        txid: Txid,
        /// We use [`SignedAmount`] because commitment transactions may be incoming or outgoing
        /// i.e. we can send or receive VTXOs.
        amount: SignedAmount,
        created_at: i64,
    },
    /// A transaction that has VTXOs as outputs.
    Ark {
        txid: Txid,
        /// We use [`SignedAmount`] because Ark transactions may be incoming or outgoing i.e.
        /// we can send or receive VTXOs.
        amount: SignedAmount,
        /// An Ark transaction is settled if our outputs in it have been spent. Thus, if we have no
        /// _outputs_ in it, it is considered settled too.
        is_settled: bool,
        created_at: i64,
    },
    /// A transaction that offboards VTXOs to an onchain output.
    Offboard {
        /// The commitment TXID that settles the VTXOs.
        commitment_txid: Txid,
        /// We use [`Amount`] because offboarding transactions are always outgoing.
        amount: Amount,
        /// Confirmation time of the commitment transaction. This information must be provided by
        /// an external source (e.g., esplora).
        confirmed_at: Option<i64>,
    },
}

impl Transaction {
    /// The creation time of the [`Transaction`]. This value can be used for sorting.
    ///
    /// - The creation time of a boarding transaction is based on its confirmation time. If it is
    ///   pending, we return [`None`].
    ///
    /// - The creation time of a commitment transaction is based on the `created_at` of our VTXO
    ///   produced by it.
    ///
    /// - The creation time of an Ark transaction is based on the `created_at` of our VTXO produced
    ///   by it.
    ///
    /// - The creation time of an offboard transaction is based on its confirmation time. If it is
    ///   pending, we return [`None`].
    pub fn created_at(&self) -> Option<i64> {
        match self {
            Transaction::Boarding { confirmed_at, .. }
            | Transaction::Offboard { confirmed_at, .. } => *confirmed_at,
            Transaction::Commitment { created_at, .. } | Transaction::Ark { created_at, .. } => {
                Some(*created_at)
            }
        }
    }

    pub fn txid(&self) -> Txid {
        match self {
            Transaction::Boarding { txid, .. }
            | Transaction::Commitment { txid, .. }
            | Transaction::Ark { txid, .. } => *txid,
            Transaction::Offboard {
                commitment_txid, ..
            } => *commitment_txid,
        }
    }
}

/// Sorts a slice of [`Transaction`] in descending order by creation time.
///
/// Transactions with no creation time (None) are placed first, followed by transactions
/// sorted by creation time in descending order (newest first).
pub fn sort_transactions_by_created_at(txs: &mut [Transaction]) {
    txs.sort_by(|a, b| match (a.created_at(), b.created_at()) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a_time), Some(b_time)) => b_time.cmp(&a_time),
    });
}

/// Generate a list of transactions where we receive VTXOs.
///
/// This list excludes settlements or transactions where we receive a change VTXO.
pub fn generate_incoming_vtxo_transaction_history(
    spent_vtxos: &[VirtualTxOutPoint],
    spendable_vtxos: &[VirtualTxOutPoint],
    // Commitment transactions which take a boarding output of ours as an input.
    boarding_commitment_txs: &[Txid],
) -> Result<Vec<Transaction>, Error> {
    let mut txs = Vec::new();

    let all_vtxos = spent_vtxos.iter().chain(spendable_vtxos.iter());

    let mut spent_vtxos_left_to_check = spent_vtxos.to_vec();

    // We iterate through every VTXO because all VTXOs were incoming at some point.
    for vtxo in all_vtxos {
        // Confirmed settlement of boarding output into VTXO => IGNORED.
        if !vtxo.is_preconfirmed
            && boarding_commitment_txs.contains(
                // There should only be one commitment TXID for confirmed VTXOs.
                &vtxo.commitment_txids[0],
            )
        {
            continue;
        }

        // An incoming VTXO that deserves an entry in the transaction history is the result of an
        // incoming payment. We may receive a VTXO as part of a commitment transaction or through an
        // Ark transaction.

        if vtxo.is_preconfirmed {
            // We compute how much we spent in that Ark transaction.
            let spent_amount = {
                let mut spent_amount = Amount::ZERO;
                let mut remaining_spent_vtxos = Vec::new();
                for spent_vtxo in spent_vtxos_left_to_check.iter() {
                    if spent_vtxo.ark_txid == Some(vtxo.outpoint.txid) {
                        spent_amount += spent_vtxo.amount;
                    } else {
                        remaining_spent_vtxos.push(spent_vtxo.clone());
                    }
                }

                spent_vtxos_left_to_check = remaining_spent_vtxos;

                spent_amount
            };

            let receive_amount = vtxo.amount.to_signed().map_err(Error::ad_hoc)?;
            let spent_amount = spent_amount.to_signed().map_err(Error::ad_hoc)?;

            let net_amount = receive_amount - spent_amount;

            // If net amount is zero, it's a self-payment => IGNORED.
            //
            // If net amount is negative, it's a change VTXO => IGNORED.
            if net_amount.is_positive() {
                txs.push(Transaction::Ark {
                    txid: vtxo.outpoint.txid,
                    amount: net_amount,
                    is_settled: vtxo.spent_by.is_some() ||
                        // To include settled dust outputs too!
                        vtxo.settled_by.is_some(),
                    created_at: vtxo.created_at,
                })
            }
        } else {
            // We compute how much we spent in that batch.
            let spent_amount = {
                let mut spent_amount = Amount::ZERO;
                let mut remaining_spent_vtxos = Vec::new();
                for spent_vtxo in spent_vtxos_left_to_check.iter() {
                    // There should only be one commitment TXID for confirmed VTXOs.
                    let commitment_txid = vtxo.commitment_txids[0];

                    if spent_vtxo.settled_by == Some(commitment_txid) {
                        spent_amount += spent_vtxo.amount;
                    } else {
                        remaining_spent_vtxos.push(spent_vtxo.clone());
                    }
                }

                spent_vtxos_left_to_check = remaining_spent_vtxos;

                spent_amount
            };

            let receive_amount = vtxo.amount.to_signed().map_err(Error::ad_hoc)?;
            let spent_amount = spent_amount.to_signed().map_err(Error::ad_hoc)?;

            let net_amount = receive_amount - spent_amount;

            // If net amount received is zero, it's a VTXO being settled => IGNORED.
            //
            // If net amount received is negative, it's a change VTXO => IGNORED.
            if net_amount.is_positive() {
                txs.push(Transaction::Commitment {
                    txid: vtxo.outpoint.txid,
                    amount: receive_amount,
                    created_at: vtxo.created_at,
                })
            }
        }
    }

    Ok(txs)
}

/// Generate a list of outgoing transactions.
///
/// This includes:
/// - Outgoing Ark transactions (offchain payments)
/// - Offboarding transactions (collaborative redeem to onchain)
///
/// Pure settlements (VTXO refreshes with no net outflow) are excluded.
///
/// # Returns
///
/// An iterator of [`OutgoingTransaction`]s.
///
/// We do not return a list of [`Transaction`]s directly because some outgoing transactions may need
/// additional data to be constructed:
/// - [`OutgoingTransaction::Incomplete`]: needs a [`VirtualTxOutPoint`] to complete.
/// - [`OutgoingTransaction::IncompleteOffboard`]: needs confirmation data from an external source.
///
/// # Example
///
/// ```rust
/// # use ark_core::history::OutgoingTransaction;
/// # use ark_core::history::generate_outgoing_vtxo_transaction_history;
/// # use ark_core::server::VirtualTxOutPoint;
/// # use ark_core::Error;
/// # use bitcoin::OutPoint;
/// # use bitcoin::Txid;
/// # fn fetch_virtual_tx_outpoint(_outpoint: OutPoint) -> Result<Option<VirtualTxOutPoint>, Error> {
/// #     Ok(None)
/// # }
/// # fn fetch_tx_confirmation_time(_txid: Txid) -> Result<Option<i64>, Error> {
/// #     Ok(None)
/// # }
/// #
/// # let spent_vtxos = vec![];
/// # let spendable_vtxos = vec![];
/// let outgoing_txs = generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos).unwrap();
///
/// let mut complete_outgoing_txs = vec![];
/// for outgoing_tx in outgoing_txs {
///     match outgoing_tx {
///         OutgoingTransaction::Complete(complete_tx) => {
///             complete_outgoing_txs.push(complete_tx);
///         }
///         OutgoingTransaction::Incomplete(incomplete_tx) => {
///             // Need to fetch additional VTXO data to complete.
///             let virtual_tx_outpoint = fetch_virtual_tx_outpoint(incomplete_tx.first_outpoint()).unwrap();
///             if let Some(virtual_tx_outpoint) = virtual_tx_outpoint {
///                 let complete_tx = incomplete_tx.finish(&virtual_tx_outpoint).unwrap();
///                 complete_outgoing_txs.push(complete_tx);
///             }
///         }
///         OutgoingTransaction::IncompleteOffboard(incomplete_offboard) => {
///             // Need to fetch confirmation time from an external source (e.g., esplora).
///             let confirmed_at = fetch_tx_confirmation_time(incomplete_offboard.commitment_txid()).unwrap();
///             let complete_tx = incomplete_offboard.finish(confirmed_at);
///             complete_outgoing_txs.push(complete_tx);
///         }
///     }
/// }
/// ```
pub fn generate_outgoing_vtxo_transaction_history(
    spent_vtxos: &[VirtualTxOutPoint],
    spendable_vtxos: &[VirtualTxOutPoint],
) -> Result<impl Iterator<Item = OutgoingTransaction>, Error> {
    let all_vtxos = [spent_vtxos, spendable_vtxos].concat();

    // We collect all the transactions where one or more VTXOs of ours are spent.
    let mut vtxos_by_spent_by = HashMap::<Txid, Vec<VirtualTxOutPoint>>::new();
    // We collect all the VTXOs that are settled (forfeited) by a commitment transaction.
    let mut vtxos_by_settled_by = HashMap::<Txid, Vec<VirtualTxOutPoint>>::new();

    for spent_vtxo in spent_vtxos.iter() {
        if let Some(settled_by) = spent_vtxo.settled_by {
            // Track settlements to detect offboarding.
            match vtxos_by_settled_by.entry(settled_by) {
                Entry::Occupied(mut occupied_entry) => {
                    occupied_entry.get_mut().push(spent_vtxo.clone());
                }
                Entry::Vacant(e) => {
                    e.insert(vec![spent_vtxo.clone()]);
                }
            }
        } else if spent_vtxo.spent_by.is_some()
            && let Some(ark_txid) = spent_vtxo.ark_txid
        {
            match vtxos_by_spent_by.entry(ark_txid) {
                Entry::Occupied(mut occupied_entry) => {
                    occupied_entry.get_mut().push(spent_vtxo.clone());
                }
                Entry::Vacant(e) => {
                    e.insert(vec![spent_vtxo.clone()]);
                }
            }
        }
    }

    // An outgoing VTXO that warrants an entry in the transaction history is the input to an
    // outgoing payment. We may send a VTXO as part of a commitment transaction or through an Ark
    // transaction.
    let mut outgoing_txs = Vec::new();

    // Process regular outgoing transactions (Ark transactions).
    for (spend_txid, spent_vtxos) in vtxos_by_spent_by.iter() {
        let spent_amount = spent_vtxos
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.amount)
            .to_signed()
            .map_err(Error::ad_hoc)?;

        let produced_virtual_tx_outpoints = all_vtxos
            .iter()
            .filter(|v| v.outpoint.txid == *spend_txid)
            .collect::<Vec<_>>();

        let produced_amount = produced_virtual_tx_outpoints
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.amount)
            .to_signed()
            .map_err(Error::ad_hoc)?;

        let net_amount = produced_amount - spent_amount;

        if !net_amount.is_negative() {
            // Ignore settlements and self-payments.
            continue;
        }

        let tx = match produced_virtual_tx_outpoints.first() {
            Some(virtual_tx_change_outpoint) => {
                OutgoingTransaction::with_change(virtual_tx_change_outpoint, net_amount)
            }
            None => OutgoingTransaction::without_change(*spend_txid, net_amount),
        };

        outgoing_txs.push(tx);
    }

    // Process settlements to detect offboarding transactions.
    //
    // When VTXOs are settled by a commitment transaction, the inputs may be:
    // 1. Refreshed into new VTXOs of equal value (pure settlement) - ignore.
    // 2. Partially offboarded with some change VTXO remaining - track the offboarded amount.
    // 3. Fully offboarded with no change VTXO - track the entire amount.
    //
    // NOTE: I believe this may not tell the whole story, but it's good enough for now.
    for (commitment_txid, settled_vtxos) in vtxos_by_settled_by.iter() {
        let input_amount = settled_vtxos
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.amount)
            .to_signed()
            .map_err(Error::ad_hoc)?;

        // Find VTXOs that were produced by this settlement (have commitment_txid in their
        // commitment_txids).
        let produced_vtxos = all_vtxos
            .iter()
            .filter(|v| v.commitment_txids.contains(commitment_txid))
            .collect::<Vec<_>>();

        let output_amount = produced_vtxos
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.amount)
            .to_signed()
            .map_err(Error::ad_hoc)?;

        let offboarded_amount = input_amount - output_amount;

        if offboarded_amount.is_positive() {
            // Some or all of the input was offboarded onchain.
            outgoing_txs.push(OutgoingTransaction::IncompleteOffboard(
                IncompleteOffboardTransaction {
                    commitment_txid: *commitment_txid,
                    amount: offboarded_amount.to_unsigned().map_err(Error::ad_hoc)?,
                },
            ));
        }
        // If offboarded_amount <= 0, it's a pure settlement (refresh) - ignore.
    }

    Ok(OutgoingTransactionIter::new(outgoing_txs))
}

/// An outgoing transaction.
///
/// If the transaction is [`OutgoingTransaction::Complete`], it can be used as is. If the
/// transaction is [`OutgoingTransaction::Incomplete`], you will need to complete it with a
/// [`VirtualTxOutPoint`]. If the transaction is [`OutgoingTransaction::IncompleteOffboard`], you
/// will need to complete it with confirmation data.
///
/// Refer to [`generate_outgoing_vtxo_transaction_history`] for more info on how to use this type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutgoingTransaction {
    Complete(Transaction),
    Incomplete(IncompleteOutgoingTransaction),
    IncompleteOffboard(IncompleteOffboardTransaction),
}

impl OutgoingTransaction {
    /// Build an outgoing transaction with a change output of ours.
    ///
    /// With the change [`VirtualTxOutPoint`], we can go ahead and build the corresponding
    /// [`Transaction`].
    fn with_change(
        virtual_tx_change_outpoint: &VirtualTxOutPoint,
        net_amount: SignedAmount,
    ) -> Self {
        Self::Complete(build_outgoing_transaction(
            virtual_tx_change_outpoint,
            net_amount,
        ))
    }

    /// Build outgoing transaction data, without a change output of ours.
    ///
    /// Without a change output, we need to look for a foreign [`VirtualTxOutPoint`] to be able to
    /// build the corresponding [`Transaction`].
    fn without_change(txid: Txid, net_amount: SignedAmount) -> Self {
        Self::Incomplete(IncompleteOutgoingTransaction {
            first_outpoint: bitcoin::OutPoint { txid, vout: 0 },
            net_amount,
        })
    }
}

/// An outgoing transaction that is missing data about one of its [`VirtualTxOutPoint`]s so that it
/// can be completed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IncompleteOutgoingTransaction {
    // We take the first one because:
    //
    // - Any outpoint will work.
    // - Every transaction has at least one outpoint.
    first_outpoint: bitcoin::OutPoint,
    net_amount: SignedAmount,
}

/// An offboard transaction that is missing confirmation data so that it can be completed.
///
/// Use [`IncompleteOffboardTransaction::finish`] to complete the transaction with confirmation
/// data from an external source (e.g., esplora).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IncompleteOffboardTransaction {
    commitment_txid: Txid,
    amount: Amount,
}

impl IncompleteOffboardTransaction {
    /// The commitment TXID of this offboard transaction.
    ///
    /// Use this value to query an external source (e.g., esplora) for confirmation data.
    pub fn commitment_txid(&self) -> Txid {
        self.commitment_txid
    }

    /// Transform this incomplete offboard transaction into a [`Transaction`].
    ///
    /// # Arguments
    ///
    /// * `confirmed_at`: The confirmation time of the commitment transaction, or [`None`] if
    ///   unconfirmed.
    pub fn finish(self, confirmed_at: Option<i64>) -> Transaction {
        Transaction::Offboard {
            commitment_txid: self.commitment_txid,
            amount: self.amount,
            confirmed_at,
        }
    }
}

impl IncompleteOutgoingTransaction {
    /// The first [`bitcoin::OutPoint`] of this transaction.
    ///
    /// Use this value to find the corresponding [`VirtualTxOutPoint`], to be able to call
    /// [`IncompleteOutgoingTransaction::finish`] and build a [`Transaction`].
    pub fn first_outpoint(&self) -> bitcoin::OutPoint {
        self.first_outpoint
    }

    /// Transform this incomplete outgoing transaction into a [`Transaction`].
    ///
    /// # Arguments
    ///
    /// * `virtual_tx_outpoint`: a [`VirtualTxOutPoint`].
    ///
    /// # Returns
    ///
    /// A complete [`Transaction`].
    ///
    /// # Errors
    ///
    /// If the TXID of the provided `virtual_tx_outpoint` does not match that of the
    /// `first_outpoint` field, we return an error.
    pub fn finish(self, virtual_tx_outpoint: &VirtualTxOutPoint) -> Result<Transaction, Error> {
        if self.first_outpoint.txid != virtual_tx_outpoint.outpoint.txid {
            return Err(Error::ad_hoc(format!(
                "cannot finish outgoing transaction with unrelated \
                virtual TX outpoint: expected {}, got {}",
                self.first_outpoint.txid, virtual_tx_outpoint.outpoint.txid
            )));
        }

        Ok(build_outgoing_transaction(
            virtual_tx_outpoint,
            self.net_amount,
        ))
    }
}

/// An iterator of [`OutgoingTransaction`]s.
struct OutgoingTransactionIter {
    inner: std::vec::IntoIter<OutgoingTransaction>,
}

impl OutgoingTransactionIter {
    /// Build a new iterator of [`OutgoingTransaction`]s.
    fn new(txs: Vec<OutgoingTransaction>) -> Self {
        Self {
            inner: txs.into_iter(),
        }
    }
}

impl Iterator for OutgoingTransactionIter {
    type Item = OutgoingTransaction;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// Build an outgoing [`Transaction`].
fn build_outgoing_transaction(
    // A virtual TX outpoint of the outgoing transaction.
    vtxo_outpoint: &VirtualTxOutPoint,
    // A negative amount representing coins received minus coins sent in the transaction.
    net_amount: SignedAmount,
) -> Transaction {
    let created_at = vtxo_outpoint.created_at;
    match vtxo_outpoint.is_preconfirmed {
        true => Transaction::Ark {
            txid: vtxo_outpoint.outpoint.txid,
            amount: net_amount,
            // For a pre-confirmed outgoing Ark transaction, the sender always considers the
            // transaction settled.
            is_settled: true,
            created_at,
        },
        false => Transaction::Commitment {
            txid: vtxo_outpoint.commitment_txids[0],
            amount: net_amount,
            created_at,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::OutPoint;
    use bitcoin::ScriptBuf;

    // These tests are taken straight from the Go client.
    // NOTE: The go tests disappeared when the client was moved to a different repository.

    #[test]
    fn alice_before_sending() {
        let boarding_commitment_txs = [
            "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
        ];

        let spendable_vtxos = [VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: "2646aea682389e1739a33a617d1f3ee28ccc7e4e16210936cece7a823e37527e"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            created_at: 1730330127,
            expires_at: 1730934927,
            amount: Amount::from_sat(20_000),
            script: ScriptBuf::new(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![
                "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                    .parse()
                    .unwrap(),
            ],
            settled_by: None,
            ark_txid: None,
        }];

        let inc_txs = generate_incoming_vtxo_transaction_history(
            &[],
            &spendable_vtxos,
            &boarding_commitment_txs,
        )
        .unwrap();

        let out_txs = generate_outgoing_vtxo_transaction_history(&[], &spendable_vtxos)
            .unwrap()
            .collect::<Vec<_>>();

        assert!(inc_txs.is_empty());
        assert!(out_txs.is_empty());
    }

    #[test]
    fn alice_after_sending() {
        let boarding_commitment_txs = [
            "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
        ];

        let spendable_vtxos = [VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
                vout: 1,
            },
            created_at: 1730330256,
            expires_at: 1730934927,
            amount: Amount::from_sat(18_784),
            script: ScriptBuf::new(),
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![
                "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                    .parse()
                    .unwrap(),
            ],
            settled_by: None,
            ark_txid: None,
        }];

        let spent_vtxos = [VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: "2646aea682389e1739a33a617d1f3ee28ccc7e4e16210936cece7a823e37527e"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            created_at: 1730330127,
            expires_at: 1730934927,
            amount: Amount::from_sat(20_000),
            script: ScriptBuf::new(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: true,
            spent_by: Some(
                "e3c4f18d0418935db8000c5b8c8fc8d776b5741cd625369eceea9aebb8bcee03"
                    .parse()
                    .unwrap(),
            ),
            commitment_txids: vec![
                "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                    .parse()
                    .unwrap(),
            ],
            settled_by: None,
            ark_txid: Some(
                "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
            ),
        }];

        let inc_txs = generate_incoming_vtxo_transaction_history(
            &spent_vtxos,
            &spendable_vtxos,
            &boarding_commitment_txs,
        )
        .unwrap();

        let out_txs = generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos)
            .unwrap()
            .filter_map(|tx| {
                if let OutgoingTransaction::Complete(tx) = tx {
                    Some(tx)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        assert!(inc_txs.is_empty());

        assert_eq!(
            out_txs,
            [Transaction::Ark {
                txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
                amount: SignedAmount::from_sat(-1_216),
                is_settled: true,
                created_at: 1730330256,
            }]
        );
    }

    #[test]
    fn bob_before_settling() {
        let spendable_vtxos = [
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330256,
                expires_at: 1730934927,
                amount: Amount::from_sat(1_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: false,
                spent_by: None,
                commitment_txids: vec![
                    "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                        .parse()
                        .unwrap(),
                ],
                settled_by: None,
                ark_txid: None,
            },
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330748,
                expires_at: 1730935548,
                amount: Amount::from_sat(2_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: false,
                spent_by: None,
                commitment_txids: vec![
                    "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                        .parse()
                        .unwrap(),
                ],
                settled_by: None,
                ark_txid: None,
            },
        ];

        let spent_vtxos = [];

        let mut inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        sort_transactions_by_created_at(&mut inc_txs);

        let out_txs = generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(
            inc_txs,
            [
                Transaction::Ark {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: false,
                    created_at: 1730330748,
                },
                Transaction::Ark {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(1_000),
                    is_settled: false,
                    created_at: 1730330256,
                }
            ]
        );

        assert!(out_txs.is_empty());
    }

    #[test]
    fn bob_after_settling() {
        let spendable_vtxos = [VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: "d9c95372c0c419fd007005edd54e21dabac0375a37fc5f17c313bc1e5f483af9"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            created_at: 1730331035,
            expires_at: 1730935835,
            amount: Amount::from_sat(3_000),
            script: ScriptBuf::new(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![
                "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                    .parse()
                    .unwrap(),
            ],
            settled_by: None,
            ark_txid: None,
        }];

        let spent_vtxos = [
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330256,
                expires_at: 1730934927,
                amount: Amount::from_sat(1_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: true,
                spent_by: Some(
                    "c9bdde5595c5479394e805a8c468657cd94ae75a504172e514030b3c549f3646"
                        .parse()
                        .unwrap(),
                ),
                commitment_txids: vec![
                    "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                        .parse()
                        .unwrap(),
                ],
                settled_by: Some(
                    "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                        .parse()
                        .unwrap(),
                ),
                ark_txid: None,
            },
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330748,
                expires_at: 1730935548,
                amount: Amount::from_sat(2_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: true,
                spent_by: Some(
                    "a7c06a495dd145fd95693a5190b26ffa391aa4440c1af26f9ff293166d97d807"
                        .parse()
                        .unwrap(),
                ),
                commitment_txids: vec![
                    "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                        .parse()
                        .unwrap(),
                ],
                settled_by: Some(
                    "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                        .parse()
                        .unwrap(),
                ),
                ark_txid: None,
            },
        ];

        let mut inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        sort_transactions_by_created_at(&mut inc_txs);

        let out_txs = generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(
            inc_txs,
            [
                Transaction::Ark {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: true,
                    created_at: 1730330748,
                },
                Transaction::Ark {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(1_000),
                    is_settled: true,
                    created_at: 1730330256,
                }
            ]
        );

        assert!(out_txs.is_empty());
    }

    #[test]
    fn bob_after_sending() {
        let spendable_vtxos = [VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: "c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55"
                    .parse()
                    .unwrap(),
                vout: 1,
            },
            created_at: 1730331198,
            expires_at: 1730935835,
            amount: Amount::from_sat(684),
            script: ScriptBuf::new(),
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![
                "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                    .parse()
                    .unwrap(),
            ],
            settled_by: None,
            ark_txid: None,
        }];

        let spent_vtxos = [
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330256,
                expires_at: 1730934927,
                amount: Amount::from_sat(1_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: true,
                spent_by: Some(
                    "c9bdde5595c5479394e805a8c468657cd94ae75a504172e514030b3c549f3646"
                        .parse()
                        .unwrap(),
                ),
                commitment_txids: vec![
                    "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                        .parse()
                        .unwrap(),
                ],
                settled_by: Some(
                    "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                        .parse()
                        .unwrap(),
                ),
                ark_txid: None,
            },
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730330748,
                expires_at: 1730935548,
                amount: Amount::from_sat(2_000),
                script: ScriptBuf::new(),
                is_preconfirmed: true,
                is_swept: false,
                is_unrolled: false,
                is_spent: true,
                spent_by: Some(
                    "a7c06a495dd145fd95693a5190b26ffa391aa4440c1af26f9ff293166d97d807"
                        .parse()
                        .unwrap(),
                ),
                commitment_txids: vec![
                    "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                        .parse()
                        .unwrap(),
                ],
                settled_by: Some(
                    "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                        .parse()
                        .unwrap(),
                ),
                ark_txid: None,
            },
            VirtualTxOutPoint {
                outpoint: OutPoint {
                    txid: "d9c95372c0c419fd007005edd54e21dabac0375a37fc5f17c313bc1e5f483af9"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                created_at: 1730331035,
                expires_at: 1730935835,
                amount: Amount::from_sat(3_000),
                script: ScriptBuf::new(),
                is_preconfirmed: false,
                is_swept: false,
                is_unrolled: false,
                is_spent: true,
                spent_by: Some(
                    "cfcfec99c9767162fc2432fac7cac6240eae2ce344d2d0e1600284399f5dd493"
                        .parse()
                        .unwrap(),
                ),
                commitment_txids: vec![
                    "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                        .parse()
                        .unwrap(),
                ],
                settled_by: None,
                ark_txid: Some(
                    "c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55"
                        .parse()
                        .unwrap(),
                ),
            },
        ];

        let inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        let out_txs = generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos)
            .unwrap()
            .filter_map(|tx| {
                if let OutgoingTransaction::Complete(tx) = tx {
                    Some(tx)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut txs = [inc_txs, out_txs].concat();
        sort_transactions_by_created_at(&mut txs);

        assert_eq!(
            txs,
            [
                Transaction::Ark {
                    txid: "c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(-2_316),
                    is_settled: true,
                    created_at: 1730331198,
                },
                Transaction::Ark {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: true,
                    created_at: 1730330748,
                },
                Transaction::Ark {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(1_000),
                    is_settled: true,
                    created_at: 1730330256,
                }
            ]
        );
    }
}
