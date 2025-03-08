use crate::script::csv_sig_script;
use crate::script::multisig_script;
use crate::script::tr_script_pubkey;
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
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::XOnlyPublicKey;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct BoardingOutput {
    server: XOnlyPublicKey,
    owner: XOnlyPublicKey,
    spend_info: TaprootSpendInfo,
    address: Address,
    exit_delay: bitcoin::Sequence,
}

impl BoardingOutput {
    pub fn new<C>(
        secp: &Secp256k1<C>,
        server: XOnlyPublicKey,
        owner: XOnlyPublicKey,
        exit_delay: bitcoin::Sequence,
        network: Network,
    ) -> Self
    where
        C: Verification,
    {
        let unspendable_key: PublicKey = UNSPENDABLE_KEY.parse().expect("valid key");
        let (unspendable_key, _) = unspendable_key.inner.x_only_public_key();

        let multisig_script = multisig_script(server, owner);
        let exit_script = csv_sig_script(exit_delay, owner);

        let spend_info = TaprootBuilder::new()
            .add_leaf(1, multisig_script)
            .expect("valid multisig leaf")
            .add_leaf(1, exit_script)
            .expect("valid exit leaf")
            .finalize(secp, unspendable_key)
            .expect("can be finalized");

        let script_pubkey = tr_script_pubkey(&spend_info);
        let address = Address::from_script(&script_pubkey, network).expect("valid script");

        Self {
            server,
            owner,
            spend_info,
            address,
            exit_delay,
        }
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    pub fn owner_pk(&self) -> XOnlyPublicKey {
        self.owner
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.address.script_pubkey()
    }

    pub fn forfeit_spend_info(&self) -> (ScriptBuf, taproot::ControlBlock) {
        // It's kind of rubbish that we need to reconstruct the script every time we want a
        // `ControlBlock`. It would be nicer to just get the `ControlBlock` for the left leaf and
        // the right leaf, knowing which one is which.

        let forfeit_script = self.forfeit_script();

        let control_block = self
            .spend_info
            .control_block(&(forfeit_script.clone(), LeafVersion::TapScript))
            .expect("forfeit script");

        (forfeit_script, control_block)
    }

    pub fn exit_spend_info(&self) -> (ScriptBuf, taproot::ControlBlock) {
        let exit_script = self.exit_script();

        let control_block = self
            .spend_info
            .control_block(&(exit_script.clone(), LeafVersion::TapScript))
            .expect("exit script");

        (exit_script, control_block)
    }

    pub fn exit_delay(&self) -> bitcoin::Sequence {
        self.exit_delay
    }

    pub fn exit_delay_duration(&self) -> Duration {
        let exit_delay = self
            .exit_delay
            .to_relative_lock_time()
            .expect("relative lock time");

        match exit_delay {
            relative::LockTime::Time(time) => Duration::from_secs(time.value() as u64 * 512),
            relative::LockTime::Blocks(_) => {
                unreachable!("Only seconds timelock is supported");
            }
        }
    }

    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        let (exit_script, _) = self.exit_spend_info();
        let (forfeit_script, _) = self.forfeit_spend_info();

        vec![exit_script, forfeit_script]
    }

    /// Whether the boarding output can be claimed unilaterally by the owner or not, given the
    /// `confirmation_blocktime` of the transaction that included this boarding output as an output.
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

/// The on-chain status of a collection of boarding outputs.
#[derive(Debug, Clone, Default)]
pub struct BoardingOutpoints {
    /// Boarding outputs that can be converted into VTXOs in collaboration with the Ark server.
    pub spendable: Vec<(OutPoint, Amount, BoardingOutput)>,
    /// Boarding outputs that should only be spent unilaterally.
    pub expired: Vec<(OutPoint, Amount, BoardingOutput)>,
    /// Boarding outputs that are not yet confirmed on-chain.
    pub pending: Vec<(OutPoint, Amount, BoardingOutput)>,
    /// Boarding outputs that were already spent.
    pub spent: Vec<(OutPoint, Amount)>,
}

impl BoardingOutpoints {
    pub fn spendable_balance(&self) -> Amount {
        self.spendable.iter().fold(Amount::ZERO, |acc, x| acc + x.1)
    }

    pub fn expired_balance(&self) -> Amount {
        self.expired.iter().fold(Amount::ZERO, |acc, x| acc + x.1)
    }

    pub fn pending_balance(&self) -> Amount {
        self.pending.iter().fold(Amount::ZERO, |acc, x| acc + x.1)
    }
}

/// Given a list of [`BoardingOutput`]s, determine their on-chain status.
pub fn list_boarding_outpoints<F>(
    find_outpoints_fn: F,
    boarding_outputs: &[BoardingOutput],
) -> Result<BoardingOutpoints, Error>
where
    F: Fn(&Address) -> Result<Vec<ExplorerUtxo>, Error>,
{
    let mut spendable = Vec::new();
    let mut expired = Vec::new();
    let mut pending = Vec::new();
    let mut spent = Vec::new();
    for boarding_output in boarding_outputs.iter() {
        let boarding_address = boarding_output.address();

        // The boarding outputs corresponding to this address that we can find on-chain.
        let boarding_utxos = find_outpoints_fn(boarding_address)?;

        for boarding_utxo in boarding_utxos.iter() {
            match *boarding_utxo {
                // The boarding output can be found on-chain.
                ExplorerUtxo {
                    confirmation_blocktime: Some(confirmation_blocktime),
                    outpoint,
                    amount,
                    is_spent: false,
                } => {
                    let now = std::time::UNIX_EPOCH.elapsed().map_err(Error::ad_hoc)?;

                    // If the boarding output is on-chain can be spent unilaterally, it has expired.
                    if boarding_output.can_be_claimed_unilaterally_by_owner(
                        now,
                        Duration::from_secs(confirmation_blocktime),
                    ) {
                        expired.push((outpoint, amount, boarding_output.clone()));
                    }
                    // If the boarding output is on-chain and cannot be spent unilaterally, it is
                    // spendable.
                    else {
                        spendable.push((outpoint, amount, boarding_output.clone()));
                    }
                }
                // The boarding output is still pending confirmation.
                ExplorerUtxo {
                    confirmation_blocktime: None,
                    outpoint,
                    amount,
                    is_spent: false,
                } => {
                    pending.push((outpoint, amount, boarding_output.clone()));
                }
                // The boarding output was spent.
                ExplorerUtxo {
                    outpoint,
                    amount,
                    is_spent: true,
                    ..
                } => spent.push((outpoint, amount)),
            }
        }
    }

    Ok(BoardingOutpoints {
        spendable,
        expired,
        pending,
        spent,
    })
}
