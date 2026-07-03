use crate::server::Info;
use crate::server::VirtualTxOutPoint;
use crate::ExplorerUtxo;
use crate::Vtxo;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct VtxoList {
    // Unspent
    pre_confirmed: Vec<VirtualTxOutPoint>,
    confirmed: Vec<VirtualTxOutPoint>,
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
        for virtual_tx_outpoint in virtual_tx_outpoints {
            if virtual_tx_outpoint.is_recoverable(dust) {
                recoverable.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_spent_status(dust) {
                spent.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_pre_confirmed_spendable(dust) {
                pre_confirmed.push(virtual_tx_outpoint);
            } else if virtual_tx_outpoint.is_confirmed_spendable(dust) {
                confirmed.push(virtual_tx_outpoint);
            }
        }

        VtxoList {
            pre_confirmed,
            confirmed,
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
            .chain(self.recoverable.iter())
    }

    /// VTXOs that are in a state that allows for unilateral exit.
    ///
    /// This does _not_ mean that the VTXOs are readily spendable on-chain, just that their ancestor
    /// chain can still be published.
    pub fn could_exit_unilaterally(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed.iter().chain(self.confirmed.iter())
    }

    /// VTXOs that can be spent in an offchain transaction.
    pub fn spendable_offchain(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed.iter().chain(self.confirmed.iter())
    }

    /// VTXOs that can be spent in an offchain transaction at `now_unix_secs`.
    ///
    /// This excludes otherwise-spendable VTXOs minted under a deprecated signer whose
    /// cooperative-sign window has closed. Those VTXOs cannot be forfeited by the server anymore;
    /// they become usable again only after they expire and move into the recovery path.
    pub fn spendable_offchain_at<'a, F>(
        &'a self,
        server_info: &'a Info,
        now_unix_secs: i64,
        server_pk_for_script: F,
    ) -> impl Iterator<Item = &'a VirtualTxOutPoint> + 'a
    where
        F: Fn(&ScriptBuf) -> Option<XOnlyPublicKey> + 'a,
    {
        self.spendable_offchain().filter(move |vtxo| {
            !server_pk_for_script(&vtxo.script)
                .map(|server_pk| server_info.signer_requires_recovery_at(server_pk, now_unix_secs))
                .unwrap_or(false)
        })
    }

    /// Otherwise-spendable VTXOs blocked only by a deprecated signer's closed cooperative-sign
    /// window. These remain wallet funds, but they are pending recovery until expiry.
    pub fn pending_recovery_due_to_signer_at<'a, F>(
        &'a self,
        server_info: &'a Info,
        now_unix_secs: i64,
        server_pk_for_script: F,
    ) -> impl Iterator<Item = &'a VirtualTxOutPoint> + 'a
    where
        F: Fn(&ScriptBuf) -> Option<XOnlyPublicKey> + 'a,
    {
        self.spendable_offchain().filter(move |vtxo| {
            server_pk_for_script(&vtxo.script)
                .map(|server_pk| server_info.signer_requires_recovery_at(server_pk, now_unix_secs))
                .unwrap_or(false)
        })
    }

    /// Unspent VTXOs that may be included in a cooperative batch settlement at `now_unix_secs`.
    ///
    /// Recoverable VTXOs are always safe: they no longer need a server forfeit signature. Healthy
    /// VTXOs still need that signature, so VTXOs under an expired deprecated signer are excluded.
    pub fn batch_settleable_at<'a, F>(
        &'a self,
        server_info: &'a Info,
        now_unix_secs: i64,
        server_pk_for_script: F,
    ) -> impl Iterator<Item = &'a VirtualTxOutPoint> + 'a
    where
        F: Fn(&ScriptBuf) -> Option<XOnlyPublicKey> + 'a,
    {
        let dust = server_info.dust;
        self.all_unspent().filter(move |vtxo| {
            vtxo.is_recoverable(dust)
                || !server_pk_for_script(&vtxo.script)
                    .map(|server_pk| {
                        server_info.signer_requires_recovery_at(server_pk, now_unix_secs)
                    })
                    .unwrap_or(false)
        })
    }

    pub fn pre_confirmed(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.pre_confirmed.iter()
    }

    pub fn confirmed(&self) -> impl Iterator<Item = &VirtualTxOutPoint> {
        self.confirmed.iter()
    }

    /// Returns the list of recoverable VTXOs
    ///
    /// A VTXO is recoverable if it:
    ///
    /// - has expired;
    /// - was swept already; or
    /// - is sub-dust.
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
                    confirmations,
                    ..
                }) => {
                    // VTXOs with an _active_ exit path. These should be claimed unilaterally.
                    if let Some(vtxo) = vtxos.get(&v.script) {
                        vtxo.can_be_claimed_unilaterally_by_owner(
                            now,
                            Duration::from_secs(*confirmation_blocktime),
                            *confirmations,
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
