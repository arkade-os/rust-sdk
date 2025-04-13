use crate::server::VtxoOutPoint;
use crate::Error;
use bitcoin::Amount;
use bitcoin::SignedAmount;
use bitcoin::Txid;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArkTransaction {
    /// A transaction that transforms a UTXO into a boarding output.
    Boarding {
        txid: Txid,
        /// We use [`Amount`] because boarding transactions are always incoming i.e. we receive a
        /// boarding output.
        amount: Amount,
        confirmed_at: Option<i64>,
    },
    /// A transaction that confirms VTXOs.
    Round {
        txid: Txid,
        /// We use [`SignedAmount`] because round transactions may be incoming or outgoing i.e. we
        /// can send or receive VTXOs.
        amount: SignedAmount,
        created_at: i64,
    },
    /// A transaction that sends VTXOs.
    Redeem {
        txid: Txid,
        /// We use [`SignedAmount`] because redeem transactions may be incoming or outgoing
        /// i.e. we can send or receive VTXOs.
        amount: SignedAmount,
        /// A redeem transaction is settled if our outputs in it have been spent.
        is_settled: bool,
        created_at: i64,
    },
}

impl ArkTransaction {
    /// The creation time of the [`ArkTransaction`]. This value can be used for sorting.
    ///
    /// - The creation time of a boarding transaction is based on its confirmation time. If it is
    ///   pending, we return [`i64::MAX`] to convey that the transaction will be "created" in the
    ///   future.
    ///
    /// - The creation time of a round transaction is based on the `created_at` of our VTXO produced
    ///   by it.
    ///
    /// - The creation time of a redeem transaction is based on the `created_at` of our VTXO
    ///   produced by it.
    pub fn created_at(&self) -> i64 {
        match self {
            ArkTransaction::Boarding { confirmed_at, .. } => confirmed_at.unwrap_or(i64::MAX),
            ArkTransaction::Round { created_at, .. } => *created_at,
            ArkTransaction::Redeem { created_at, .. } => *created_at,
        }
    }
}

/// Generate a list of _relevant_ transactions where we receive VTXOs.
///
/// Relevant transactions exclude settlements.
pub fn generate_incoming_vtxo_transaction_history(
    spent_vtxos: &[VtxoOutPoint],
    spendable_vtxos: &[VtxoOutPoint],
    // Round transactions which take a boarding output of ours as an input.
    boarding_round_txs: &[Txid],
) -> Result<Vec<ArkTransaction>, Error> {
    let mut txs = Vec::new();

    let all_vtxos = spent_vtxos.iter().chain(spendable_vtxos.iter());

    let mut spent_vtxos_left_to_check = spent_vtxos.to_vec();

    // We iterate through every VTXO because all VTXOs were incoming at some point.
    for vtxo in all_vtxos {
        // Confirmed settlement of boarding output into VTXO => IGNORED.
        //
        // TODO: What if we have more than one incoming VTXO in the same round? I think this could
        // be wrong.
        if boarding_round_txs.contains(&vtxo.round_txid) && !vtxo.is_pending {
            continue;
        }

        // An incoming VTXO that warrants an entry in the transaction history is the result of an
        // incoming payment. We may receive a VTXO within a round transaction or via a redeem
        // transaction.

        if vtxo.is_pending {
            // We compute how much we spent in that redeem transaction.
            let spent_amount = {
                let mut spent_amount = Amount::ZERO;
                let mut remaining_spent_vtxos = Vec::new();
                for spent_vtxo in spent_vtxos_left_to_check.iter() {
                    if spent_vtxo.spent_by == Some(vtxo.outpoint.txid) {
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

            // If net amount is zero, it's a VTXO being settled (OOR, weird) => IGNORED.
            //
            // If net amount is negative, it's a change VTXO => IGNORED.
            if net_amount.is_positive() {
                txs.push(ArkTransaction::Redeem {
                    txid: vtxo.outpoint.txid,
                    amount: net_amount,
                    is_settled: vtxo.spent_by.is_some(),
                    created_at: vtxo.created_at,
                })
            }
        } else {
            // We compute how much we spent in that round.
            let spent_amount = {
                let mut spent_amount = Amount::ZERO;
                let mut remaining_spent_vtxos = Vec::new();
                for spent_vtxo in spent_vtxos_left_to_check.iter() {
                    if spent_vtxo.spent_by == Some(vtxo.round_txid) {
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
                txs.push(ArkTransaction::Round {
                    txid: vtxo.outpoint.txid,
                    amount: receive_amount,
                    created_at: vtxo.created_at,
                })
            }
        }
    }

    Ok(txs)
}

/// Generate a list of _relevant_ transactions where we send VTXOs.
///
/// By relevant transactions we mean everything except for settlements.
pub fn generate_outgoing_vtxo_transaction_history(
    spent_vtxos: &[VtxoOutPoint],
    spendable_vtxos: &[VtxoOutPoint],
) -> Result<Vec<ArkTransaction>, Error> {
    let mut txs = Vec::new();

    let all_vtxos = [spent_vtxos, spendable_vtxos].concat();

    // We collect all the transactions where one or more VTXOs of ours are spent.
    let mut vtxos_by_spent_by = HashMap::<Txid, Vec<VtxoOutPoint>>::new();
    for spent_vtxo in spent_vtxos.iter() {
        if let Some(spend_txid) = spent_vtxo.spent_by {
            match vtxos_by_spent_by.entry(spend_txid) {
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
    // outgoing payment. We may send a VTXO within a round transaction or via a redeem transaction.

    enum TxType {
        Round,
        Redeem,
    }

    for (spend_txid, spent_vtxos) in vtxos_by_spent_by.iter() {
        let spent_amount = spent_vtxos
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.amount)
            .to_signed()
            .map_err(Error::ad_hoc)?;

        // We figure out the transaction type based on the produced VTXOs.
        let tx_type = all_vtxos.iter().find_map(|vtxo| {
            if vtxo.round_txid == *spend_txid {
                Some(TxType::Round)
            } else if vtxo.outpoint.txid == *spend_txid {
                Some(TxType::Redeem)
            } else {
                None
            }
        });

        match tx_type {
            Some(TxType::Round) => {
                let produced_vtxos = all_vtxos
                    .iter()
                    .filter(|v| v.round_txid == *spend_txid)
                    .collect::<Vec<_>>();

                let produced_amount = produced_vtxos
                    .iter()
                    .fold(Amount::ZERO, |acc, x| acc + x.amount)
                    .to_signed()
                    .map_err(Error::ad_hoc)?;

                // TODO: Sending own VTXO to own address is not handled correctly.

                let net_amount = produced_amount - spent_amount;

                // If net amount is zero, it's a VTXO being settled => IGNORED.
                //
                // If net amount is positive, it's a change VTXO => IGNORED.
                if net_amount.is_negative() {
                    txs.push(ArkTransaction::Round {
                        txid: *spend_txid,
                        amount: net_amount,
                        created_at: produced_vtxos[0].created_at,
                    })
                }
            }
            Some(TxType::Redeem) => {
                let produced_vtxos = all_vtxos
                    .iter()
                    .filter(|v| v.outpoint.txid == *spend_txid)
                    .collect::<Vec<_>>();

                let produced_amount = produced_vtxos
                    .iter()
                    .fold(Amount::ZERO, |acc, x| acc + x.amount)
                    .to_signed()
                    .map_err(Error::ad_hoc)?;

                // TODO: Sending own VTXO to own address is not handled correctly.

                let net_amount = produced_amount - spent_amount;

                // If net amount is zero, it's a VTXO being settled (OOR, weird) => IGNORED.
                //
                // If net amount is positive, it's a change VTXO => IGNORED.
                if net_amount.is_negative() {
                    txs.push(ArkTransaction::Redeem {
                        txid: *spend_txid,
                        amount: net_amount,
                        is_settled: true,
                        created_at: produced_vtxos[0].created_at,
                    })
                }
            }
            None => {
                // TODO: Using the creation time of the _spent_ VTXO is not accurate.
                let created_at = spent_vtxos[0].created_at;

                if spent_vtxos[0].is_pending {
                    txs.push(ArkTransaction::Redeem {
                        txid: *spend_txid,
                        amount: -spent_amount,
                        is_settled: true,
                        created_at,
                    });
                } else {
                    // TODO: We don't have enough information to definitively decide between
                    // `Redeem` or `Round` if this transaction spends a confirmed VTXO. For that we
                    // need to look at an output, but we don't own any of the outputs of this
                    // transaction.

                    txs.push(ArkTransaction::Round {
                        txid: *spend_txid,
                        amount: -spent_amount,
                        created_at,
                    });
                }
            }
        }
    }

    Ok(txs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::OutPoint;

    // These tests are taken straight from the Go client.

    #[test]
    fn alice_before_sending() {
        let boarding_round_txs = [
            "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
        ];

        let spendable_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "2646aea682389e1739a33a617d1f3ee28ccc7e4e16210936cece7a823e37527e"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: false,
            round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
            spent_by: None,
            expire_at: 1730934927,
            swept: false,
            is_pending: false,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(20_000),
            pubkey: "fc3ed4822401bc75858c6a7e08a974c68a777bcf87e6ba535d48afab7d00cf5f"
                .parse()
                .unwrap(),
            created_at: 1730330127,
        }];

        let inc_txs =
            generate_incoming_vtxo_transaction_history(&[], &spendable_vtxos, &boarding_round_txs)
                .unwrap();

        let out_txs = generate_outgoing_vtxo_transaction_history(&[], &spendable_vtxos).unwrap();

        assert!(inc_txs.is_empty());
        assert!(out_txs.is_empty());
    }

    #[test]
    fn alice_after_sending() {
        let boarding_round_txs = [
            "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
        ];

        let spendable_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
                vout: 1,
            },
            spent: false,
            round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
            spent_by: Some(
                "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
            ),
            expire_at: 1730934927,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(18_784),
            pubkey: "fc3ed4822401bc75858c6a7e08a974c68a777bcf87e6ba535d48afab7d00cf5f"
                .parse()
                .unwrap(),
            created_at: 1730330256,
        }];

        let spent_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "2646aea682389e1739a33a617d1f3ee28ccc7e4e16210936cece7a823e37527e"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: true,
            round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
            spent_by: Some(
                "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
            ),
            expire_at: 1730934927,
            swept: false,
            is_pending: false,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(20_000),
            pubkey: "fc3ed4822401bc75858c6a7e08a974c68a777bcf87e6ba535d48afab7d00cf5f"
                .parse()
                .unwrap(),
            created_at: 1730330127,
        }];

        let inc_txs = generate_incoming_vtxo_transaction_history(
            &spent_vtxos,
            &spendable_vtxos,
            &boarding_round_txs,
        )
        .unwrap();

        let out_txs =
            generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos).unwrap();

        assert!(inc_txs.is_empty());

        assert_eq!(
            out_txs,
            [ArkTransaction::Redeem {
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
        let spendable_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: false,
            round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
            spent_by: None,
            expire_at: 1730934927,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(1_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730330256,
        }, VtxoOutPoint {
            outpoint: OutPoint {
                txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: false,
            round_txid: "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                .parse()
                .unwrap(),
            spent_by: None,
            expire_at: 1730935548,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(2_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730330748,
        }];

        let spent_vtxos = [];

        let mut inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        inc_txs.sort_by_key(|b| std::cmp::Reverse(b.created_at()));

        let out_txs =
            generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos).unwrap();

        assert_eq!(
            inc_txs,
            [
                ArkTransaction::Redeem {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: false,
                    created_at: 1730330748,
                },
                ArkTransaction::Redeem {
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
        let spendable_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "d9c95372c0c419fd007005edd54e21dabac0375a37fc5f17c313bc1e5f483af9"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: false,
            round_txid: "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                .parse()
                .unwrap(),
            spent_by: None,
            expire_at: 1730935835,
            swept: false,
            is_pending: false,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(3_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730331035,
        }];

        let spent_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: true,
            round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                .parse()
                .unwrap(),
            spent_by: Some("7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4".parse().unwrap()),
            expire_at: 1730934927,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(1_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730330256,
        }, VtxoOutPoint {
            outpoint: OutPoint {
                txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: true,
            round_txid: "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                .parse()
                .unwrap(),
            spent_by: Some("7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4".parse().unwrap()),
            expire_at: 1730935548,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(2_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730330748,
        }, VtxoOutPoint {
            outpoint: OutPoint {
                txid: "d9c95372c0c419fd007005edd54e21dabac0375a37fc5f17c313bc1e5f483af9"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            spent: true,
            round_txid: "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                .parse()
                .unwrap(),
            spent_by: Some("c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55".parse().unwrap()),
            expire_at: 1730935835,
            swept: false,
            is_pending: false,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(3_000),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730331035,
        }];

        let mut inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        inc_txs.sort_by_key(|b| std::cmp::Reverse(b.created_at()));

        let out_txs =
            generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos).unwrap();

        assert_eq!(
            inc_txs,
            [
                ArkTransaction::Redeem {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: true,
                    created_at: 1730330748,
                },
                ArkTransaction::Redeem {
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
        let spendable_vtxos = [VtxoOutPoint {
            outpoint: OutPoint {
                txid: "c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55"
                    .parse()
                    .unwrap(),
                vout: 1,
            },
            spent: false,
            round_txid: "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                .parse()
                .unwrap(),
            spent_by: None,
            expire_at: 1730935835,
            swept: false,
            is_pending: true,
            redeem_txs: Vec::new(),
            amount: Amount::from_sat(684),
            pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                .parse()
                .unwrap(),
            created_at: 1730331198,
        }];

        let spent_vtxos = [
            VtxoOutPoint {
                outpoint: OutPoint {
                    txid: "33fd8ca9ea9cfb53802c42be10ae428573e19fb89484dfe536d06d43efa82034"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                spent: true,
                round_txid: "c16ae0d917ac400790da18456015975521bec6e1d1962ad728c0070808c564e8"
                    .parse()
                    .unwrap(),
                spent_by: Some("7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4".parse().unwrap()),
                expire_at: 1730934927,
                swept: false,
                is_pending: true,
                redeem_txs: Vec::new(),
                amount: Amount::from_sat(1_000),
                pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                    .parse()
                    .unwrap(),
                created_at: 1730330256,
            },
            VtxoOutPoint {
                outpoint: OutPoint {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                spent: true,
                round_txid: "a4e91c211398e0be0edad322fb74a739b1c77bb82b9e4ea94b0115b8e4dfe645"
                    .parse()
                    .unwrap(),
                spent_by: Some("7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4".parse().unwrap()),
                expire_at: 1730935548,
                swept: false,
                is_pending: true,
                redeem_txs: Vec::new(),
                amount: Amount::from_sat(2_000),
                pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                    .parse()
                    .unwrap(),
                created_at: 1730330748,
            },
            VtxoOutPoint {
                outpoint: OutPoint {
                    txid: "d9c95372c0c419fd007005edd54e21dabac0375a37fc5f17c313bc1e5f483af9"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                spent: true,
                round_txid: "7fd65ce87e0f9a7af583593d5b0124aabd65c97e05159525d0a98201d6ae95a4"
                    .parse()
                    .unwrap(),
                spent_by: Some("c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55".parse().unwrap()),
                expire_at: 1730935835,
                swept: false,
                is_pending: false,
                redeem_txs: Vec::new(),
                amount: Amount::from_sat(3_000),
                pubkey: "aa586eb052ebf1e676f6df4eaf3591c90d56a979871aefa9b67e5618cb7c0f25"
                    .parse()
                    .unwrap(),
                created_at: 1730331035,
            }
        ];

        let inc_txs =
            generate_incoming_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos, &[])
                .unwrap();

        let out_txs =
            generate_outgoing_vtxo_transaction_history(&spent_vtxos, &spendable_vtxos).unwrap();

        let mut txs = [inc_txs, out_txs].concat();
        txs.sort_by_key(|b| std::cmp::Reverse(b.created_at()));

        assert_eq!(
            txs,
            [
                ArkTransaction::Redeem {
                    txid: "c59004f8c468a922216f513ec7d63d9b6a13571af0bacd51910709351d27fe55"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(-2_316),
                    is_settled: true,
                    created_at: 1730331198,
                },
                ArkTransaction::Redeem {
                    txid: "884d85c0db6b52139c39337d54c1f20cd8c5c0d2e83109d69246a345ccc9d169"
                        .parse()
                        .unwrap(),
                    amount: SignedAmount::from_sat(2_000),
                    is_settled: true,
                    created_at: 1730330748,
                },
                ArkTransaction::Redeem {
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
