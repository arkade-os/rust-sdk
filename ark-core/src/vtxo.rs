use crate::ark_address::ArkAddress;
use crate::script::csv_sig_script;
use crate::script::multisig_script;
use crate::script::tr_script_pubkey;
use crate::Error;
use crate::ErrorContext;
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
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::time::Duration;

/// All the information needed to _spend_ a VTXO.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Vtxo {
    server_forfeit: XOnlyPublicKey,
    owner: XOnlyPublicKey,
    spend_info: TaprootSpendInfo,
    /// All the scripts in this VTXO's Taproot tree.
    tapscripts: Vec<ScriptBuf>,
    address: Address,
    exit_delay: bitcoin::Sequence,
    exit_delay_seconds: u64,
    network: Network,
}

impl Vtxo {
    /// 64 bytes per pubkey.
    pub const FORFEIT_WITNESS_SIZE: usize = 64 * 2;

    /// Build a VTXO, by providing all the scripts to be included in the Taproot tree.
    ///
    /// The provided `scripts` must follow the following rules:
    ///
    /// - All unilateral spend paths MUST be timelocked.
    /// - All other spend paths MUST involve the Ark server's signature.
    pub fn new_with_custom_scripts<C>(
        secp: &Secp256k1<C>,
        server_forfeit: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        // TODO: Verify the validity of these scripts before constructing the `Vtxo`.
        scripts: Vec<ScriptBuf>,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Result<Self, Error>
    where
        C: Verification,
    {
        let unspendable_key: PublicKey = UNSPENDABLE_KEY
            .parse()
            .map_err(|e| Error::ad_hoc(format!("invalid unspendable key: {e}")))?;
        let (unspendable_key, _) = unspendable_key.inner.x_only_public_key();

        let leaf_distribution = calculate_leaf_depths(scripts.len());

        let mut builder = TaprootBuilder::new();
        for (script, depth) in scripts.iter().zip(leaf_distribution.iter()) {
            builder = builder
                .add_leaf(*depth as u8, script.clone())
                .map_err(Error::ad_hoc)?;
        }

        let spend_info = builder
            .finalize(secp, unspendable_key)
            .map_err(|_| Error::ad_hoc("failed to finalize Taproot tree"))?;

        let exit_delay_seconds = match exit_delay.to_relative_lock_time() {
            Some(relative::LockTime::Time(time)) => time.value() as u64 * 512,
            _ => unreachable!("VTXO redeem script must use relative lock time in seconds"),
        };

        let script_pubkey = tr_script_pubkey(&spend_info);
        let address = Address::from_script(&script_pubkey, network)
            .map_err(|e| Error::ad_hoc(format!("invalid script: {e}")))?;

        Ok(Self {
            server_forfeit,
            owner,
            spend_info,
            tapscripts: scripts,
            address,
            exit_delay,
            exit_delay_seconds,
            network,
        })
    }

    /// Build a default VTXO.
    pub fn new_default<C>(
        secp: &Secp256k1<C>,
        server_signer: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Result<Self, Error>
    where
        C: Verification,
    {
        let forfeit_script = multisig_script(server_signer, owner);
        let redeem_script = csv_sig_script(exit_delay, owner);

        Self::new_with_custom_scripts(
            secp,
            server_signer,
            owner,
            vec![forfeit_script, redeem_script],
            exit_delay,
            network,
        )
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
        self.server_forfeit
    }

    pub fn exit_delay(&self) -> bitcoin::Sequence {
        self.exit_delay
    }

    pub fn exit_delay_duration(&self) -> Duration {
        Duration::from_secs(self.exit_delay_seconds)
    }

    pub fn to_ark_address(&self) -> ArkAddress {
        let vtxo_tap_key = self.spend_info.output_key();
        ArkAddress::new(self.network, self.server_forfeit, vtxo_tap_key)
    }

    /// The spend info of an arbitrary branch of a VTXO.
    pub fn get_spend_info(&self, script: ScriptBuf) -> Result<taproot::ControlBlock, Error> {
        let control_block = self
            .spend_info
            .control_block(&(script, LeafVersion::TapScript))
            .ok_or(Error::ad_hoc("could not build control block for script"))?;

        Ok(control_block)
    }

    /// The spend info for the forfeit branch of a _default_ VTXO.
    ///
    /// This method can fail because [`Vtxo`]s constructed with the method
    /// [`Vtxo::new_with_custom_scripts`] may not contain this script exactly.
    pub fn forfeit_spend_info(&self) -> Result<(ScriptBuf, taproot::ControlBlock), Error> {
        let forfeit_script = multisig_script(self.server_forfeit, self.owner);

        let control_block = self
            .get_spend_info(forfeit_script.clone())
            .context("missing default forfeit script")?;

        Ok((forfeit_script, control_block))
    }

    /// The spend info for the unilateral exit branch of a _default_ VTXO.
    ///
    /// This method can fail because [`Vtxo`]s constructed with the method
    /// [`Vtxo::new_with_custom_scripts`] may not contain this script exactly.
    pub fn exit_spend_info(&self) -> Result<(ScriptBuf, taproot::ControlBlock), Error> {
        let exit_script = csv_sig_script(self.exit_delay, self.owner);

        let control_block = self
            .get_spend_info(exit_script.clone())
            .context("missing default exit script")?;

        Ok((exit_script, control_block))
    }

    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        self.tapscripts.clone()
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
}

fn calculate_leaf_depths(n: usize) -> Vec<usize> {
    // Handle edge cases
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![0]; // A single node has depth 0
    }
    if n == 2 {
        return vec![1, 1];
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
