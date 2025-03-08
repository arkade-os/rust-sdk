use crate::ark_address::ArkAddress;
use crate::script::csv_sig_script;
use crate::script::multisig_script;
use crate::script::tr_script_pubkey;
use crate::server::VtxoOutPoint;
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
use std::time::Duration;

/// All the information needed to _spend_ a VTXO.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
            Some(relative::LockTime::Time(time)) => time.value() * 512,
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
            exit_delay_seconds: exit_delay_seconds as u64,
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

/// The status of a collection of VTXOs.
#[derive(Debug, Clone, Default)]
pub struct VirtualTxOutpoints {
    /// VTXOs that can be spent in collaboration with the Ark server.
    pub spendable: Vec<(VtxoOutPoint, Vtxo)>,
    /// VTXOs that should only be spent unilaterally.
    pub expired: Vec<(VtxoOutPoint, Vtxo)>,
}

impl VirtualTxOutpoints {
    pub fn spendable_balance(&self) -> Amount {
        self.spendable
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.0.amount)
    }

    pub fn expired_balance(&self) -> Amount {
        self.expired
            .iter()
            .fold(Amount::ZERO, |acc, x| acc + x.0.amount)
    }
}

pub fn list_virtual_tx_outpoints<F>(
    find_outpoints_fn: F,
    spendable_vtxos: HashMap<Vtxo, Vec<VtxoOutPoint>>,
) -> Result<VirtualTxOutpoints, Error>
where
    F: Fn(&Address) -> Result<Vec<ExplorerUtxo>, Error>,
{
    let mut spendable = Vec::new();
    let mut expired = Vec::new();
    for (vtxo, virtual_tx_outpoints) in spendable_vtxos {
        // We look to see if we can find any on-chain VTXOs for this address.
        let onchain_vtxos = find_outpoints_fn(vtxo.address())?;

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
                    ..
                }) if vtxo.can_be_claimed_unilaterally_by_owner(
                    now,
                    Duration::from_secs(*confirmation_blocktime),
                ) =>
                {
                    expired.push((virtual_tx_outpoint, vtxo.clone()));
                }
                // All other VTXOs (either still offchain or on-chain but with an inactive exit
                // path) are spendable.
                _ => {
                    spendable.push((virtual_tx_outpoint, vtxo.clone()));
                }
            }
        }
    }

    Ok(VirtualTxOutpoints { spendable, expired })
}
