use crate::ark_address::ArkAddress;
use crate::script::csv_sig_script;
use crate::script::multisig_3_of_3_script;
use crate::script::multisig_script;
use crate::script::tr_script_pubkey;
use crate::Error;
use crate::ErrorContext;
use crate::ExitDelayKind;
use crate::UNSPENDABLE_KEY;
use bitcoin::key::PublicKey;
use bitcoin::key::Secp256k1;
use bitcoin::key::Verification;
use bitcoin::taproot;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::NodeInfo;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::collections::VecDeque;
use std::time::Duration;

/// All the information needed to _spend_ a VTXO.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Vtxo {
    server_forfeit: XOnlyPublicKey,
    owner: XOnlyPublicKey,
    owner_unilateral_exit: XOnlyPublicKey,
    /// The delegator's public key, if this VTXO has a delegate spending path.
    delegator: Option<XOnlyPublicKey>,
    spend_info: TaprootSpendInfo,
    /// All the scripts in this VTXO's Taproot tree.
    tapscripts: Vec<ScriptBuf>,
    address: Address,
    exit_delay: bitcoin::Sequence,
    exit_delay_kind: ExitDelayKind,
    network: Network,
}

impl Vtxo {
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
        let vtxo = Self::new_with_custom_scripts_and_split_owner_keys(
            secp,
            server_forfeit,
            owner,
            owner,
            scripts,
            exit_delay,
            network,
        )?;

        Ok(vtxo)
    }

    pub fn new_with_custom_scripts_and_split_owner_keys<C>(
        secp: &Secp256k1<C>,
        server_forfeit: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        owner_unilateral_exit: XOnlyPublicKey,
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

        let node = assemble_taproot_tree(&scripts)?;
        let spend_info = TaprootSpendInfo::from_node_info(secp, unspendable_key, node);

        let exit_delay_kind = ExitDelayKind::from_sequence(exit_delay)?;

        let script_pubkey = tr_script_pubkey(&spend_info);
        let address = Address::from_script(&script_pubkey, network)
            .map_err(|e| Error::ad_hoc(format!("invalid script: {e}")))?;

        Ok(Self {
            server_forfeit,
            owner,
            owner_unilateral_exit,
            delegator: None,
            spend_info,
            tapscripts: scripts,
            address,
            exit_delay,
            exit_delay_kind,
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

    /// Build a VTXO with a delegate spending path.
    ///
    /// This creates a 3-leaf Taproot tree:
    /// 1. **Forfeit**: 2-of-2 multisig (server + owner)
    /// 2. **Exit**: CSV-timelocked owner signature
    /// 3. **Delegate**: 3-of-3 multisig (owner + delegator + server)
    ///
    /// The delegate path allows a third-party delegator service to cooperate with the owner and
    /// the server to renew VTXOs before they expire.
    pub fn new_with_delegator<C>(
        secp: &Secp256k1<C>,
        server_signer: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        delegator: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Result<Self, Error>
    where
        C: Verification,
    {
        let forfeit_script = multisig_script(server_signer, owner);
        let redeem_script = csv_sig_script(exit_delay, owner);
        let delegate_script = multisig_3_of_3_script(owner, delegator, server_signer);

        let mut vtxo = Self::new_with_custom_scripts(
            secp,
            server_signer,
            owner,
            vec![forfeit_script, redeem_script, delegate_script],
            exit_delay,
            network,
        )?;

        vtxo.delegator = Some(delegator);

        Ok(vtxo)
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

    pub fn delegator_pk(&self) -> Option<XOnlyPublicKey> {
        self.delegator
    }

    pub fn exit_delay(&self) -> bitcoin::Sequence {
        self.exit_delay
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
        let exit_script = csv_sig_script(self.exit_delay, self.owner_unilateral_exit);

        let control_block = self
            .get_spend_info(exit_script.clone())
            .context("missing default exit script")?;

        Ok((exit_script, control_block))
    }

    /// The spend info for the delegate branch of a VTXO constructed with
    /// [`Vtxo::new_with_delegator`].
    ///
    /// Returns an error if the VTXO was not built with a delegator.
    pub fn delegate_spend_info(&self) -> Result<(ScriptBuf, taproot::ControlBlock), Error> {
        let delegator = self
            .delegator
            .ok_or(Error::ad_hoc("VTXO has no delegate path"))?;

        let delegate_script = multisig_3_of_3_script(self.owner, delegator, self.server_forfeit);

        let control_block = self
            .get_spend_info(delegate_script.clone())
            .context("missing delegate script")?;

        Ok((delegate_script, control_block))
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
        confirmations: u64,
    ) -> bool {
        match self.exit_delay_kind {
            ExitDelayKind::Time(seconds) => {
                let exit_path_time = confirmation_blocktime + seconds;

                now > exit_path_time
            }
            ExitDelayKind::Blocks(confirmations_required) => {
                confirmations >= confirmations_required
            }
        }
    }
}

/// Assemble a Taproot tree from `scripts`, in input order, matching btcd's
/// `txscript.AssembleTaprootScriptTree`.
///
/// This is the same construction the Arkade TypeScript SDK's `VtxoScript` uses
/// (`assembleBtcdTaprootTree`), so VTXOs built here derive byte-identical
/// addresses and control blocks across SDKs and the Ark server, for any number
/// of leaves. A previous depth-table approach only matched for one, two, or
/// three leaves and silently diverged (or failed) for larger trees.
///
/// The algorithm has two phases:
///
/// 1. Pair leaves left-to-right. A lone trailing leaf (odd leaf count) is merged into the branch
///    built immediately before it.
/// 2. Repeatedly merge the two front-most branches (FIFO) until a single root remains.
///
/// Branch hashing is BIP341 (lexicographically sorted children), handled by
/// [`NodeInfo::combine`], so the resulting Merkle root is independent of the
/// left/right order within each branch.
fn assemble_taproot_tree(scripts: &[ScriptBuf]) -> Result<NodeInfo, Error> {
    if scripts.is_empty() {
        return Err(Error::ad_hoc(
            "cannot build a Taproot tree from zero scripts",
        ));
    }

    let leaf =
        |script: &ScriptBuf| NodeInfo::new_leaf_with_ver(script.clone(), LeafVersion::TapScript);
    let combine = |a: NodeInfo, b: NodeInfo| {
        NodeInfo::combine(a, b)
            .map_err(|e| Error::ad_hoc(format!("failed to combine Taproot nodes: {e}")))
    };

    if scripts.len() == 1 {
        return Ok(leaf(&scripts[0]));
    }

    // Phase 1: pair leaves left-to-right.
    let mut branches: Vec<NodeInfo> = Vec::new();
    let mut i = 0;
    while i < scripts.len() {
        if i == scripts.len() - 1 {
            // Odd trailing leaf: merge it into the last branch built so far.
            let last = branches
                .pop()
                .expect("a branch is always built before a trailing odd leaf");
            branches.push(combine(last, leaf(&scripts[i]))?);
        } else {
            branches.push(combine(leaf(&scripts[i]), leaf(&scripts[i + 1]))?);
        }
        i += 2;
    }

    // Phase 2: FIFO-merge branches until a single root remains.
    let mut queue: VecDeque<NodeInfo> = branches.into();
    while queue.len() >= 2 {
        let left = queue
            .pop_front()
            .expect("queue holds at least two branches");
        let right = queue
            .pop_front()
            .expect("queue holds at least two branches");
        queue.push_back(combine(left, right)?);
    }

    Ok(queue
        .pop_front()
        .expect("exactly one root branch remains after merging"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::Secp256k1;
    use std::str::FromStr;

    fn test_keys() -> (XOnlyPublicKey, XOnlyPublicKey, XOnlyPublicKey) {
        let server = XOnlyPublicKey::from_str(
            "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let owner = XOnlyPublicKey::from_str(
            "28845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let delegator = XOnlyPublicKey::from_str(
            "38845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        (server, owner, delegator)
    }

    #[test]
    fn new_with_delegator_has_three_tapscripts() {
        let secp = Secp256k1::new();
        let (server, owner, delegator) = test_keys();
        let exit_delay = bitcoin::Sequence::from_seconds_ceil(86400).unwrap();

        let vtxo = Vtxo::new_with_delegator(
            &secp,
            server,
            owner,
            delegator,
            exit_delay,
            Network::Regtest,
        )
        .unwrap();

        assert_eq!(vtxo.tapscripts().len(), 3);
        assert_eq!(vtxo.delegator_pk(), Some(delegator));
    }

    #[test]
    fn delegator_vtxo_all_spend_paths_resolve() {
        let secp = Secp256k1::new();
        let (server, owner, delegator) = test_keys();
        let exit_delay = bitcoin::Sequence::from_seconds_ceil(86400).unwrap();

        let vtxo = Vtxo::new_with_delegator(
            &secp,
            server,
            owner,
            delegator,
            exit_delay,
            Network::Regtest,
        )
        .unwrap();

        // All three spend paths should produce valid spend info.
        let (forfeit_script, _cb) = vtxo.forfeit_spend_info().unwrap();
        let (exit_script, _cb) = vtxo.exit_spend_info().unwrap();
        let (delegate_script, _cb) = vtxo.delegate_spend_info().unwrap();

        // Scripts should be distinct.
        assert_ne!(forfeit_script, exit_script);
        assert_ne!(forfeit_script, delegate_script);
        assert_ne!(exit_script, delegate_script);
    }

    #[test]
    fn default_vtxo_has_no_delegate_path() {
        let secp = Secp256k1::new();
        let (server, owner, _) = test_keys();
        let exit_delay = bitcoin::Sequence::from_seconds_ceil(86400).unwrap();

        let vtxo = Vtxo::new_default(&secp, server, owner, exit_delay, Network::Regtest).unwrap();

        assert!(vtxo.delegator_pk().is_none());
        assert!(vtxo.delegate_spend_info().is_err());
    }

    #[test]
    fn delegator_vtxo_address_differs_from_default() {
        let secp = Secp256k1::new();
        let (server, owner, delegator) = test_keys();
        let exit_delay = bitcoin::Sequence::from_seconds_ceil(86400).unwrap();

        let default =
            Vtxo::new_default(&secp, server, owner, exit_delay, Network::Regtest).unwrap();
        let with_delegator = Vtxo::new_with_delegator(
            &secp,
            server,
            owner,
            delegator,
            exit_delay,
            Network::Regtest,
        )
        .unwrap();

        // Different taproot trees should produce different addresses.
        assert_ne!(default.address(), with_delegator.address());
    }

    fn xonly(secp: &Secp256k1<bitcoin::secp256k1::All>, byte: u8) -> XOnlyPublicKey {
        bitcoin::key::Keypair::from_seckey_slice(secp, &[byte; 32])
            .expect("non-zero repeated byte is a valid secret key")
            .x_only_public_key()
            .0
    }

    /// `n` distinct leaf scripts (single-key CSV) for tree-shape tests.
    fn leaf_scripts(secp: &Secp256k1<bitcoin::secp256k1::All>, n: usize) -> Vec<ScriptBuf> {
        let seq = bitcoin::Sequence::from_seconds_ceil(512)
            .expect("512 seconds is a valid relative timelock");
        (0..n)
            .map(|i| csv_sig_script(seq, xonly(secp, (i as u8) + 1)))
            .collect()
    }

    /// `new_with_custom_scripts` must reproduce btcd's `AssembleTaprootScriptTree`
    /// (and therefore the TS SDK's `VtxoScript`). We cross-check against an
    /// independent `TaprootBuilder` reconstruction using the known btcd leaf
    /// depths, in input order, for the counts the Ark flow actually uses.
    #[test]
    fn custom_scripts_tree_matches_btcd_assembly() {
        use bitcoin::taproot::TaprootBuilder;

        let secp = Secp256k1::new();
        let (server, owner, _) = test_keys();
        let exit = bitcoin::Sequence::from_seconds_ceil(512)
            .expect("512 seconds is a valid relative timelock");

        // btcd depth tables (input order) for 2..=5 leaves.
        let cases: &[(usize, &[u8])] = &[
            (2, &[1, 1]),
            (3, &[2, 2, 1]),
            (4, &[2, 2, 2, 2]),
            (5, &[2, 2, 3, 3, 2]),
        ];

        let unspendable: PublicKey = UNSPENDABLE_KEY
            .parse()
            .expect("hardcoded unspendable key is valid");
        let (unspendable, _) = unspendable.inner.x_only_public_key();

        for (n, depths) in cases {
            let scripts = leaf_scripts(&secp, *n);

            let vtxo = Vtxo::new_with_custom_scripts(
                &secp,
                server,
                owner,
                scripts.clone(),
                exit,
                Network::Bitcoin,
            )
            .expect("custom scripts build a valid VTXO");

            let mut builder = TaprootBuilder::new();
            for (script, depth) in scripts.iter().zip(depths.iter()) {
                builder = builder
                    .add_leaf(*depth, script.clone())
                    .expect("valid leaf depth for reference tree");
            }
            let reference = builder
                .finalize(&secp, unspendable)
                .expect("reference taproot tree finalizes");
            let reference = ArkAddress::new(Network::Bitcoin, server, reference.output_key());

            assert_eq!(
                vtxo.to_ark_address().encode(),
                reference.encode(),
                "tree mismatch for {n} leaves"
            );
        }
    }

    /// Every leaf must remain spendable (resolve a control block) for any leaf
    /// count, including the non-power-of-two counts the old depth table either
    /// mis-built (5) or failed on entirely (4, 6).
    #[test]
    fn custom_scripts_tree_preserves_all_leaves() {
        let secp = Secp256k1::new();
        let (server, owner, _) = test_keys();
        let exit = bitcoin::Sequence::from_seconds_ceil(512)
            .expect("512 seconds is a valid relative timelock");

        for n in 1..=16usize {
            let scripts = leaf_scripts(&secp, n);
            let vtxo = Vtxo::new_with_custom_scripts(
                &secp,
                server,
                owner,
                scripts.clone(),
                exit,
                Network::Bitcoin,
            )
            .expect("custom scripts build a valid VTXO");

            assert_eq!(vtxo.tapscripts().len(), n, "leaf count changed for n={n}");
            for script in &scripts {
                assert!(
                    vtxo.get_spend_info(script.clone()).is_ok(),
                    "no control block for a leaf with n={n}"
                );
            }
        }
    }

    /// Known-answer test pinning the 5-leaf escrow address to the value produced
    /// by the Arkade TS SDK / `arkade.computer` operator. Guards cross-SDK
    /// address compatibility against future tree-construction regressions.
    #[test]
    fn escrow_five_leaf_address_matches_ts_sdk() {
        use bitcoin::absolute::LockTime;
        use bitcoin::opcodes::all::OP_CHECKSIG;
        use bitcoin::opcodes::all::OP_CHECKSIGVERIFY;
        use bitcoin::opcodes::all::OP_CLTV;
        use bitcoin::opcodes::all::OP_CSV;
        use bitcoin::opcodes::all::OP_DROP;
        use bitcoin::script::Builder;

        let secp = Secp256k1::new();
        let (server, arbiter, alice, bob) = (
            xonly(&secp, 1),
            xonly(&secp, 2),
            xonly(&secp, 3),
            xonly(&secp, 4),
        );
        let expiry = LockTime::from_consensus(1_750_000_000);
        let exit = bitcoin::Sequence::from_seconds_ceil(512)
            .expect("512 seconds is a valid relative timelock");

        let cltv = |a: XOnlyPublicKey, b: XOnlyPublicKey| {
            Builder::new()
                .push_int(expiry.to_consensus_u32() as i64)
                .push_opcode(OP_CLTV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&a)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&b)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        };
        let csv = |a: XOnlyPublicKey, b: XOnlyPublicKey| {
            Builder::new()
                .push_int(exit.to_consensus_u32() as i64)
                .push_opcode(OP_CSV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&a)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&b)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        };

        let scripts = vec![
            multisig_3_of_3_script(server, arbiter, alice),
            multisig_3_of_3_script(server, arbiter, bob),
            cltv(server, arbiter),
            multisig_3_of_3_script(server, alice, bob),
            csv(alice, bob),
        ];

        let vtxo =
            Vtxo::new_with_custom_scripts(&secp, server, alice, scripts, exit, Network::Bitcoin)
                .expect("escrow scripts build a valid VTXO");

        assert_eq!(
            vtxo.to_ark_address().encode(),
            "ark1qqdcf32k0vfxgsyet5ldt246q4jaw8scx3sysx0lnstlt6w4m5rclv2zhfqtr8zgxsw3erprwh0z7vg6uyg75n229y8kn0d4yjew75z35ddvjs",
        );
    }
}
