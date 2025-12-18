use crate::server::VirtualTxOutPoint;
use crate::ExplorerUtxo;
use crate::Vtxo;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct VtxoList {
    // Unspent
    pre_confirmed: Vec<VirtualTxOutPoint>,
    confirmed: Vec<VirtualTxOutPoint>,
    expired: Vec<VirtualTxOutPoint>,
    recoverable: Vec<VirtualTxOutPoint>,

    // Spent
    spent: Vec<VirtualTxOutPoint>,
}

impl VtxoList {
    pub fn new(
        // The dust amount according to the Arkade server. Dust outputs are considered recoverable.
        dust: Amount,
        virtual_tx_outpoints: Vec<VirtualTxOutPoint>,
    ) -> Self {
        let mut recoverable = Vec::new();
        let mut spent = Vec::new();
        let mut pre_confirmed = Vec::new();
        let mut confirmed = Vec::new();
        let mut expired = Vec::new();
        for virtual_tx_outpoint in virtual_tx_outpoints {
            if virtual_tx_outpoint.is_recoverable(dust) {
                recoverable.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_unrolled
                || virtual_tx_outpoint.is_spent
                || virtual_tx_outpoint.is_swept
            {
                spent.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_expired() {
                expired.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_preconfirmed {
                pre_confirmed.push(virtual_tx_outpoint);
            } else {
                confirmed.push(virtual_tx_outpoint);
            }
        }

        VtxoList {
            pre_confirmed,
            confirmed,
            expired,
            recoverable,
            spent,
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.all_unspent().chain(self.spent())
    }

    pub fn all_unspent(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed
            .iter()
            .chain(self.confirmed.iter())
            .chain(self.expired.iter())
            .chain(self.recoverable.iter())
    }

    /// VTXOs that are in a state that allows for unilateral exit.
    ///
    /// This does _not_ mean that the VTXOs are readily spendable on-chain, just that their ancestor
    /// chain can still be published.
    pub fn could_exit_unilaterally(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed
            .iter()
            .chain(self.confirmed.iter())
            .chain(self.expired.iter())
    }

    /// VTXOs that can be spent in an offchain transaction.
    pub fn spendable_offchain(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed.iter().chain(self.confirmed.iter())
    }

    pub fn pre_confirmed(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed.iter()
    }

    pub fn confirmed(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.confirmed.iter()
    }

    pub fn expired(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.expired.iter()
    }

    pub fn recoverable(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.recoverable.iter()
    }

    /// VTXOs that are already on-chain and can be spent unilaterally (the exit path is active).
    pub fn exit_ready(
        &self,
        now: Duration,
        // Corresponds to every VTXO in `vtxos` which has been found on the blockchain.
        explorer_utxos: Vec<ExplorerUtxo>,
        // TODO: We probably shouldn't involve the opinionated `Vtxo` type here.
        vtxos: HashMap<ScriptBuf, Vtxo>,
    ) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.all_unspent().filter(move |v| {
            match explorer_utxos
                .iter()
                .find(|explorer_utxo| explorer_utxo.outpoint == v.outpoint)
            {
                // VTXOs that have been confirmed on the blockchain.
                Some(ExplorerUtxo {
                    confirmation_blocktime: Some(confirmation_blocktime),
                    ..
                }) => {
                    // VTXOs with an _active_ exit path. These should be claimed unilaterally.
                    if let Some(vtxo) = vtxos.get(&v.script) {
                        vtxo.can_be_claimed_unilaterally_by_owner(
                            now,
                            Duration::from_secs(*confirmation_blocktime),
                        )
                    } else {
                        false
                    }
                }
                _ => false,
            }
        })
    }

    pub fn spent(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.spent.iter()
    }
}
