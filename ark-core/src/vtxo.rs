use crate::ark_address::ArkAddress;
use crate::script::csv_sig_script;
use crate::script::multisig_script;
use crate::script::tr_script_pubkey;
use crate::server::VirtualTxOutPoint;
use crate::Error;
use crate::ExplorerUtxo;
use crate::UNSPENDABLE_KEY;
use bitcoin::key::PublicKey;
use bitcoin::key::Secp256k1;
use bitcoin::key::Verification;
use bitcoin::relative;
use bitcoin::taproot;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::time::Duration;

/// All the information needed to _spend_ a VTXO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vtxo {
    server: XOnlyPublicKey,
    owner: XOnlyPublicKey,
    spend_info: TaprootSpendInfo,
    extra_scripts: Vec<ScriptBuf>,
    address: Address,
    exit_delay: bitcoin::Sequence,
    exit_delay_seconds: u64,
    network: Network,
}

impl Vtxo {
    /// 64 bytes per pubkey.
    pub const FORFEIT_WITNESS_SIZE: usize = 64 * 2;

    /// Build a VTXO.
    ///
    /// The `extra_scripts` argument allows for additional spend paths. All unilateral spend paths
    /// must be timelocked. Any other spend path must involve the Ark server.
    pub fn new<C>(
        secp: &Secp256k1<C>,
        server: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        // TODO: Verify the validity of these scripts before constructing the `Vtxo`.
        extra_scripts: Vec<ScriptBuf>,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Result<Self, Error>
    where
        C: Verification,
    {
        let unspendable_key: PublicKey = UNSPENDABLE_KEY.parse().expect("valid key");
        let (unspendable_key, _) = unspendable_key.inner.x_only_public_key();

        let forfeit_script = multisig_script(server, owner);
        let redeem_script = csv_sig_script(exit_delay, owner);

        let spend_info = if extra_scripts.is_empty() {
            TaprootBuilder::new()
                .add_leaf(1, forfeit_script)
                .expect("valid forfeit leaf")
                .add_leaf(1, redeem_script)
                .expect("valid redeem leaf")
                .finalize(secp, unspendable_key)
                .expect("can be finalized")
        } else {
            let scripts = [vec![forfeit_script, redeem_script], extra_scripts.clone()].concat();

            let leaf_distribution = calculate_leaf_depths(scripts.len());

            if leaf_distribution.len() == scripts.len() {
                return Err(Error::ad_hoc("wrong leaf distribution calculated"));
            }

            let mut builder = TaprootBuilder::new();
            for (script, depth) in scripts.iter().zip(leaf_distribution.iter()) {
                builder = builder
                    .add_leaf(*depth as u8, script.clone())
                    .map_err(Error::ad_hoc)?;
            }

            builder
                .finalize(secp, unspendable_key)
                .map_err(|_| Error::ad_hoc("failed to finalize Taproot tree"))?
        };

        let exit_delay_seconds = match exit_delay.to_relative_lock_time() {
            Some(relative::LockTime::Time(time)) => time.value() as u64 * 512,
            _ => unreachable!("VTXO redeem script must use relative lock time in seconds"),
        };

        let script_pubkey = tr_script_pubkey(&spend_info);
        let address = Address::from_script(&script_pubkey, network).expect("valid script");

        Ok(Self {
            server,
            owner,
            spend_info,
            extra_scripts,
            address,
            exit_delay,
            exit_delay_seconds,
            network,
        })
    }

    /// Build a default VTXO.
    pub fn new_default<C>(
        secp: &Secp256k1<C>,
        server: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Result<Self, Error>
    where
        C: Verification,
    {
        Self::new(secp, server, owner, Vec::new(), exit_delay, network)
    }

    pub fn spend_info(&self) -> &TaprootSpendInfo {
        &self.spend_info
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.address.script_pubkey()
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    pub fn owner_pk(&self) -> XOnlyPublicKey {
        self.owner
    }

    pub fn server_pk(&self) -> XOnlyPublicKey {
        self.server
    }

    pub fn exit_delay(&self) -> bitcoin::Sequence {
        self.exit_delay
    }

    pub fn exit_delay_duration(&self) -> Duration {
        Duration::from_secs(self.exit_delay_seconds)
    }

    pub fn to_ark_address(&self) -> ArkAddress {
        let vtxo_tap_key = self.spend_info.output_key();
        ArkAddress::new(self.network, self.server, vtxo_tap_key)
    }

    /// The spend info of an arbitrary branch of a VTXO.
    pub fn get_spend_info(&self, script: ScriptBuf) -> Result<taproot::ControlBlock, Error> {
        let control_block = self
            .spend_info
            .control_block(&(script, LeafVersion::TapScript))
            .expect("forfeit script");

        Ok(control_block)
    }

    /// The spend info for the forfeit branch of a VTXO.
    pub fn forfeit_spend_info(&self) -> (ScriptBuf, taproot::ControlBlock) {
        let forfeit_script = self.forfeit_script();

        let control_block = self
            .spend_info
            .control_block(&(forfeit_script.clone(), LeafVersion::TapScript))
            .expect("forfeit script");

        (forfeit_script, control_block)
    }

    /// The spend info for the unilateral exit branch of a VTXO.
    pub fn exit_spend_info(&self) -> (ScriptBuf, taproot::ControlBlock) {
        let exit_script = self.exit_script();

        let control_block = self
            .spend_info
            .control_block(&(exit_script.clone(), LeafVersion::TapScript))
            .expect("exit script");

        (exit_script, control_block)
    }

    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        let (exit_script, _) = self.exit_spend_info();
        let (forfeit_script, _) = self.forfeit_spend_info();

        let mut scripts = vec![exit_script, forfeit_script];
        scripts.append(&mut self.extra_scripts.clone());

        scripts
    }

    /// Whether the VTXO can be claimed unilaterally by the owner or not, given the
    /// `confirmation_blocktime` of the transaction that included this VTXO as an output.
    pub fn can_be_claimed_unilaterally_by_owner(
        &self,
        now: Duration,
        confirmation_blocktime: Duration,
    ) -> bool {
        let exit_path_time = confirmation_blocktime + self.exit_delay_duration();

        now > exit_path_time
    }

    fn forfeit_script(&self) -> ScriptBuf {
        multisig_script(self.server, self.owner)
    }

    fn exit_script(&self) -> ScriptBuf {
        csv_sig_script(self.exit_delay, self.owner)
    }
}

impl Hash for Vtxo {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

fn calculate_leaf_depths(n: usize) -> Vec<usize> {
    // Handle edge cases
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![0]; // A single node has depth 0
    }

    // Calculate the minimum depth required for n leaves
    let min_depth = (n as f64).log2().ceil() as usize;

    // Calculate the number of nodes at the deepest level
    let nodes_at_max_depth = n - (1 << (min_depth - 1)) + 1;
    let nodes_at_min_depth = (1 << min_depth) - nodes_at_max_depth;

    // Create the result vector with the appropriate depths
    let mut result = Vec::with_capacity(n);

    // Add the deeper nodes first
    for _ in 0..nodes_at_max_depth {
        result.push(min_depth);
    }

    // Add the less deep nodes
    for _ in 0..nodes_at_min_depth {
        result.push(min_depth - 1);
    }

    result
}

/// A collection of [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
///
/// All the information comes from the Ark server.
#[derive(Debug, Clone, Default)]
pub struct ServerVtxoList(HashMap<Vtxo, Vec<VirtualTxOutPoint>>);

impl ServerVtxoList {
    pub fn new(map: HashMap<Vtxo, Vec<VirtualTxOutPoint>>) -> Self {
        Self(map)
    }

    pub fn merge(&mut self, other: HashMap<Vtxo, Vec<VirtualTxOutPoint>>) {
        self.0.extend(other);
    }

    pub fn spendable_offchain(&self) -> HashMap<&Vtxo, Vec<&VirtualTxOutPoint>> {
        self.0
            .iter()
            .filter_map(|(vtxo, outpoints)| {
                let spendable: Vec<&VirtualTxOutPoint> = outpoints
                    .iter()
                    .filter(|vout| vout.is_spendable())
                    .collect();

                if spendable.is_empty() {
                    None
                } else {
                    Some((vtxo, spendable))
                }
            })
            .collect()
    }

    pub fn pending_spendable_offchain_balance(&self) -> Amount {
        self.spendable_offchain()
            .iter()
            .fold(Amount::ZERO, |acc, (_, x)| {
                let fold = x.iter().fold(Amount::ZERO, |acc, x| {
                    if x.is_preconfirmed {
                        acc + x.amount
                    } else {
                        acc
                    }
                });
                acc + fold
            })
    }

    pub fn confirmed_spendable_offchain_balance(&self) -> Amount {
        self.spendable_offchain()
            .iter()
            .fold(Amount::ZERO, |acc, (_, x)| {
                let fold = x.iter().fold(Amount::ZERO, |acc, x| {
                    if !x.is_preconfirmed {
                        acc + x.amount
                    } else {
                        acc
                    }
                });
                acc + fold
            })
    }

    pub fn recoverable(&self) -> HashMap<&Vtxo, Vec<&VirtualTxOutPoint>> {
        self.0
            .iter()
            .filter_map(|(vtxo, outpoints)| {
                let recoverable: Vec<&VirtualTxOutPoint> =
                    outpoints.iter().filter(|o| o.is_recoverable()).collect();

                if recoverable.is_empty() {
                    None
                } else {
                    Some((vtxo, recoverable))
                }
            })
            .collect()
    }

    pub fn recoverable_balance(&self) -> Amount {
        self.recoverable().iter().fold(Amount::ZERO, |acc, (_, x)| {
            let fold = x.iter().fold(Amount::ZERO, |acc, x| acc + x.amount);
            acc + fold
        })
    }

    pub fn to_vtxo_list<F>(self, find_outpoints_fn: F) -> Result<VtxoList, Error>
    where
        F: Fn(&Address) -> Result<Vec<ExplorerUtxo>, Error>,
    {
        let mut offchain = HashMap::<_, Vec<_>>::new();
        let mut onchain = HashMap::<_, Vec<_>>::new();
        let mut expired = HashMap::<_, Vec<_>>::new();
        for (vtxo, virtual_tx_outpoints) in self.0.into_iter() {
            // We look to see if we can find any on-chain VTXOs for this address.
            let onchain_vtxos = find_outpoints_fn(vtxo.address())?;

            Self::categorize_virtual_tx_outpoints(
                &mut offchain,
                &mut onchain,
                &mut expired,
                vtxo,
                virtual_tx_outpoints,
                onchain_vtxos,
            )?;
        }

        Ok(VtxoList {
            offchain,
            onchain,
            expired,
        })
    }

    pub async fn to_vtxo_list_async<FO, F>(self, find_outpoints_fn: FO) -> Result<VtxoList, Error>
    where
        FO: Fn(&Address) -> F,
        F: Future<Output = Result<Vec<ExplorerUtxo>, Error>>,
    {
        let mut offchain = HashMap::<_, Vec<_>>::new();
        let mut onchain = HashMap::<_, Vec<_>>::new();
        let mut expired = HashMap::<_, Vec<_>>::new();
        for (vtxo, virtual_tx_outpoints) in self.0.into_iter() {
            // We look to see if we can find any on-chain VTXOs for this address.
            let onchain_vtxos = find_outpoints_fn(vtxo.address()).await?;

            Self::categorize_virtual_tx_outpoints(
                &mut offchain,
                &mut onchain,
                &mut expired,
                vtxo,
                virtual_tx_outpoints,
                onchain_vtxos,
            )?;
        }

        Ok(VtxoList {
            offchain,
            onchain,
            expired,
        })
    }

    fn categorize_virtual_tx_outpoints(
        offchain: &mut HashMap<Vtxo, Vec<VirtualTxOutPoint>>,
        onchain: &mut HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>>,
        expired: &mut HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>>,
        vtxo: Vtxo,
        virtual_tx_outpoints: Vec<VirtualTxOutPoint>,
        onchain_vtxos: Vec<ExplorerUtxo>,
    ) -> Result<(), Error> {
        for virtual_tx_outpoint in virtual_tx_outpoints {
            let now = std::time::UNIX_EPOCH.elapsed().map_err(Error::ad_hoc)?;

            match onchain_vtxos
                .iter()
                .find(|onchain_utxo| onchain_utxo.outpoint == virtual_tx_outpoint.outpoint)
            {
                // VTXOs that have been confirmed on the blockchain, but whose
                // exit path is now _active_, have expired.
                Some(ExplorerUtxo {
                    confirmation_blocktime: Some(confirmation_blocktime),
                    is_spent,
                    ..
                }) => {
                    let onchain_virtual_tx_outpoint = OnchainVirtualTxOutPoint {
                        inner: virtual_tx_outpoint,
                        confirmation_blocktime: *confirmation_blocktime,
                        is_spent: *is_spent,
                    };

                    if vtxo.can_be_claimed_unilaterally_by_owner(
                        now,
                        Duration::from_secs(*confirmation_blocktime),
                    ) {
                        match expired.get_mut(&vtxo) {
                            Some(e) => e.push(onchain_virtual_tx_outpoint),
                            None => {
                                expired.insert(vtxo.clone(), vec![onchain_virtual_tx_outpoint]);
                            }
                        }
                    } else {
                        match onchain.get_mut(&vtxo) {
                            Some(e) => e.push(onchain_virtual_tx_outpoint),
                            None => {
                                onchain.insert(vtxo.clone(), vec![onchain_virtual_tx_outpoint]);
                            }
                        }
                    }
                }
                // All other VTXOs are offchain (we include mempool VTXOs here).
                _ => match offchain.get_mut(&vtxo) {
                    Some(e) => e.push(virtual_tx_outpoint),
                    None => {
                        offchain.insert(vtxo.clone(), vec![virtual_tx_outpoint]);
                    }
                },
            }
        }

        Ok(())
    }
}

/// A collection of [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
///
/// The information comes from both the Ark server and the blockchain.
#[derive(Debug, Clone, Default)]
pub struct VtxoList {
    /// VTXOs that remain off-chain. This is the standard state of a VTXO.
    offchain: HashMap<Vtxo, Vec<VirtualTxOutPoint>>,
    /// VTXOs that have been published on the blockchain. Unilateral exit is not yet possible.
    onchain: HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>>,
    /// VTXOs that have been published on the blockchain and can be claimed unilaterally by their
    /// owner.
    expired: HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>>,
}

impl VtxoList {
    // Getters.

    /// Get all (offchain) spendable [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
    pub fn spendable(&self) -> HashMap<&Vtxo, Vec<&VirtualTxOutPoint>> {
        self.offchain
            .iter()
            .filter_map(|(vtxo, outpoints)| {
                let spendable: Vec<&VirtualTxOutPoint> = outpoints
                    .iter()
                    .filter(|vout| vout.is_spendable())
                    .collect();

                if spendable.is_empty() {
                    None
                } else {
                    Some((vtxo, spendable))
                }
            })
            .collect()
    }

    /// Get all recoverable [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
    ///
    /// Recoverable VTXOs must be settled before they can be used. They include sub-dust VTXOs and
    /// VTXOs that were swept by the Ark server before they were settled.
    pub fn recoverable(&self) -> HashMap<&Vtxo, Vec<&VirtualTxOutPoint>> {
        self.offchain
            .iter()
            .filter_map(|(vtxo, outpoints)| {
                let recoverable: Vec<&VirtualTxOutPoint> =
                    outpoints.iter().filter(|o| o.is_recoverable()).collect();

                if recoverable.is_empty() {
                    None
                } else {
                    Some((vtxo, recoverable))
                }
            })
            .collect()
    }

    /// Get all spendable and all recoverable [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
    pub fn spendable_and_recoverable(&self) -> HashMap<&Vtxo, Vec<&VirtualTxOutPoint>> {
        self.offchain
            .iter()
            .filter_map(|(vtxo, outpoints)| {
                let list: Vec<&VirtualTxOutPoint> = outpoints
                    .iter()
                    .filter(|o| o.is_spendable() || o.is_recoverable())
                    .collect();

                if list.is_empty() {
                    None
                } else {
                    Some((vtxo, list))
                }
            })
            .collect()
    }

    /// Get all onchain [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
    ///
    /// Onchain VTXOs have been published on the blockchain, but cannot be claimed unilaterally yet.
    pub fn onchain(&self) -> &HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>> {
        &self.onchain
    }

    /// Get all (onchain) expired [`VirtualTxOutPoint`]s, indexed by [`Vtxo`].
    ///
    /// Expired VTXOs have been published on the blockchain, and can be claimed unilaterally by
    /// their owner.
    pub fn expired(&self) -> &HashMap<Vtxo, Vec<OnchainVirtualTxOutPoint>> {
        &self.expired
    }

    // Balance.

    /// Get all the (offchain) spendable VTXO balance.
    pub fn spendable_balance(&self) -> Amount {
        let (pre, con) = self.spendable_offchain_balance_aux();

        pre + con
    }

    /// Get the (offchain) pre-confirmed spendable VTXO balance.
    pub fn pending_spendable_offchain_balance(&self) -> Amount {
        let (pre, _) = self.spendable_offchain_balance_aux();

        pre
    }

    /// Get the (offchain) confirmed spendable VTXO balance.
    pub fn confirmed_spendable_offchain_balance(&self) -> Amount {
        let (_, con) = self.spendable_offchain_balance_aux();

        con
    }

    fn spendable_offchain_balance_aux(&self) -> (Amount, Amount) {
        self.spendable().iter().fold(
            (Amount::ZERO, Amount::ZERO),
            |(acc_pre, acc_con), (_, x)| {
                let fold = x
                    .iter()
                    .fold((Amount::ZERO, Amount::ZERO), |(acc_pre, acc_con), x| {
                        if x.is_preconfirmed {
                            (acc_pre + x.amount, acc_con)
                        } else {
                            (acc_pre, acc_con + x.amount)
                        }
                    });

                (acc_pre + fold.0, acc_con + fold.1)
            },
        )
    }

    /// Get the recoverable VTXO balance.
    pub fn recoverable_balance(&self) -> Amount {
        self.recoverable().iter().fold(Amount::ZERO, |acc, (_, x)| {
            let fold = x.iter().fold(Amount::ZERO, |acc, x| acc + x.amount);
            acc + fold
        })
    }

    /// Get the onchain VTXO balance.
    pub fn onchain_balance(&self) -> Amount {
        self.onchain().iter().fold(Amount::ZERO, |acc, (_, x)| {
            let fold = x.iter().fold(Amount::ZERO, |acc, x| acc + x.inner.amount);
            acc + fold
        })
    }

    /// Get the (onchain) expired VTXO balance.
    pub fn expired_balance(&self) -> Amount {
        self.expired().iter().fold(Amount::ZERO, |acc, (_, x)| {
            let fold = x.iter().fold(Amount::ZERO, |acc, x| acc + x.inner.amount);
            acc + fold
        })
    }

    // Just the `VirtualTxOutPoint`s; not indexed by `Vtxo`.

    /// Get all (offchain) spendable [`VirtualTxOutPoint`]s.
    pub fn spendable_outpoints(&self) -> Vec<VirtualTxOutPoint> {
        self.spendable()
            .iter()
            .flat_map(|(_, os)| os)
            .copied()
            .cloned()
            .collect()
    }

    /// Get all (offchain, onchain, expired) spent [`VirtualTxOutPoint`]s.
    pub fn spent_outpoints(&self) -> Vec<VirtualTxOutPoint> {
        let offchain = self
            .offchain
            .iter()
            .filter_map(|(_, outpoints)| {
                let spent: Vec<&VirtualTxOutPoint> =
                    outpoints.iter().filter(|vout| vout.is_spent()).collect();

                if spent.is_empty() {
                    None
                } else {
                    Some(spent)
                }
            })
            .flatten();

        let onchain = self
            .onchain
            .iter()
            .filter_map(|(_, outpoints)| {
                let spent: Vec<&VirtualTxOutPoint> = outpoints
                    .iter()
                    .filter_map(|v| v.inner.is_spent().then_some(&v.inner))
                    .collect();

                if spent.is_empty() {
                    None
                } else {
                    Some(spent)
                }
            })
            .flatten();

        let expired = self
            .expired
            .iter()
            .filter_map(|(_, outpoints)| {
                let spent: Vec<&VirtualTxOutPoint> = outpoints
                    .iter()
                    .filter_map(|v| v.inner.is_spent().then_some(&v.inner))
                    .collect();

                if spent.is_empty() {
                    None
                } else {
                    Some(spent)
                }
            })
            .flatten();

        offchain.chain(onchain).chain(expired).cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub struct OnchainVirtualTxOutPoint {
    inner: VirtualTxOutPoint,
    confirmation_blocktime: u64,
    is_spent: bool,
}
