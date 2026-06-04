use crate::boarding_output::BoardingOutput;
use crate::vhtlc::VhtlcOptions;
use crate::vtxo::Vtxo;
use crate::Error;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::All;
use bitcoin::taproot::ControlBlock;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContractType(String);

impl ContractType {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty() {
            return Err(Error::ad_hoc("contract type cannot be empty"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn default_vtxo() -> Self {
        Self("default".to_string())
    }

    pub fn delegate_vtxo() -> Self {
        Self("delegate".to_string())
    }

    pub fn boarding() -> Self {
        Self("boarding".to_string())
    }

    pub fn vhtlc() -> Self {
        Self("vhtlc".to_string())
    }
}

impl fmt::Display for ContractType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&'static str> for ContractType {
    fn from(value: &'static str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractState {
    Active,
    Inactive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredContract {
    pub contract_type: ContractType,
    pub contract_version: u32,
    pub script_pubkey: ScriptBuf,
    pub state: ContractState,
    pub created_at: u64,
    pub key_index: Option<u32>,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ContractView {
    pub contract: StoredContract,
    pub address: Option<Address>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendPath {
    pub name: String,
    pub script: ScriptBuf,
    pub control_block: Option<ControlBlock>,
}

impl SpendPath {
    pub fn new(name: impl Into<String>, script: ScriptBuf, control_block: ControlBlock) -> Self {
        Self {
            name: name.into(),
            script,
            control_block: Some(control_block),
        }
    }
}

#[derive(Clone)]
pub struct ContractContext {
    network: Network,
    secp: Secp256k1<All>,
}

impl ContractContext {
    pub fn new(network: Network) -> Self {
        Self {
            network,
            secp: Secp256k1::new(),
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn secp(&self) -> &Secp256k1<All> {
        &self.secp
    }
}

pub trait ContractSpec:
    Clone + Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static
{
    const VERSION: u32;

    fn contract_type() -> ContractType;
    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error>;
    fn spendable_paths(&self, ctx: &ContractContext) -> Result<Vec<SpendPath>, Error>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultVtxoContract {
    pub server: XOnlyPublicKey,
    pub owner: XOnlyPublicKey,
    pub exit_delay: Sequence,
}

impl ContractSpec for DefaultVtxoContract {
    const VERSION: u32 = 1;

    fn contract_type() -> ContractType {
        ContractType::default_vtxo()
    }

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error> {
        Ok(self.vtxo(ctx)?.script_pubkey())
    }

    fn spendable_paths(&self, ctx: &ContractContext) -> Result<Vec<SpendPath>, Error> {
        let vtxo = self.vtxo(ctx)?;
        let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info()?;
        let (exit_script, exit_control_block) = vtxo.exit_spend_info()?;
        Ok(vec![
            SpendPath::new("forfeit", forfeit_script, forfeit_control_block),
            SpendPath::new("exit", exit_script, exit_control_block),
        ])
    }
}

impl DefaultVtxoContract {
    fn vtxo(&self, ctx: &ContractContext) -> Result<Vtxo, Error> {
        Vtxo::new_default(
            ctx.secp(),
            self.server,
            self.owner,
            self.exit_delay,
            ctx.network(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateVtxoContract {
    pub server: XOnlyPublicKey,
    pub owner: XOnlyPublicKey,
    pub delegator: XOnlyPublicKey,
    pub exit_delay: Sequence,
}

impl ContractSpec for DelegateVtxoContract {
    const VERSION: u32 = 1;

    fn contract_type() -> ContractType {
        ContractType::delegate_vtxo()
    }

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error> {
        Ok(self.vtxo(ctx)?.script_pubkey())
    }

    fn spendable_paths(&self, ctx: &ContractContext) -> Result<Vec<SpendPath>, Error> {
        let vtxo = self.vtxo(ctx)?;
        let (forfeit_script, forfeit_control_block) = vtxo.forfeit_spend_info()?;
        let (exit_script, exit_control_block) = vtxo.exit_spend_info()?;
        let (delegate_script, delegate_control_block) = vtxo.delegate_spend_info()?;
        Ok(vec![
            SpendPath::new("forfeit", forfeit_script, forfeit_control_block),
            SpendPath::new("exit", exit_script, exit_control_block),
            SpendPath::new("delegate", delegate_script, delegate_control_block),
        ])
    }
}

impl DelegateVtxoContract {
    fn vtxo(&self, ctx: &ContractContext) -> Result<Vtxo, Error> {
        Vtxo::new_with_delegator(
            ctx.secp(),
            self.server,
            self.owner,
            self.delegator,
            self.exit_delay,
            ctx.network(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardingContract {
    pub server: XOnlyPublicKey,
    pub owner: XOnlyPublicKey,
    pub exit_delay: Sequence,
}

impl ContractSpec for BoardingContract {
    const VERSION: u32 = 1;

    fn contract_type() -> ContractType {
        ContractType::boarding()
    }

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error> {
        Ok(self.boarding_output(ctx)?.script_pubkey())
    }

    fn spendable_paths(&self, ctx: &ContractContext) -> Result<Vec<SpendPath>, Error> {
        let boarding_output = self.boarding_output(ctx)?;
        let (forfeit_script, forfeit_control_block) = boarding_output.forfeit_spend_info();
        let (exit_script, exit_control_block) = boarding_output.exit_spend_info();
        Ok(vec![
            SpendPath::new("forfeit", forfeit_script, forfeit_control_block),
            SpendPath::new("exit", exit_script, exit_control_block),
        ])
    }
}

impl BoardingContract {
    pub fn boarding_output(&self, ctx: &ContractContext) -> Result<BoardingOutput, Error> {
        BoardingOutput::new(
            ctx.secp(),
            self.server,
            self.owner,
            self.exit_delay,
            ctx.network(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VhtlcContract {
    pub options: VhtlcOptions,
}

impl ContractSpec for VhtlcContract {
    const VERSION: u32 = 1;

    fn contract_type() -> ContractType {
        ContractType::vhtlc()
    }

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error> {
        let script = crate::vhtlc::VhtlcScript::new(self.options.clone(), ctx.network())
            .map_err(|e| Error::ad_hoc(format!("failed to build vhtlc: {e}")))?;
        Ok(script.script_pubkey())
    }

    fn spendable_paths(&self, ctx: &ContractContext) -> Result<Vec<SpendPath>, Error> {
        let script = crate::vhtlc::VhtlcScript::new(self.options.clone(), ctx.network())
            .map_err(|e| Error::ad_hoc(format!("failed to build vhtlc: {e}")))?;
        Ok(script
            .get_script_map()
            .into_iter()
            .map(|(name, tapscript)| {
                let control_block = script
                    .taproot_spend_info()
                    .control_block(&(tapscript.clone(), bitcoin::taproot::LeafVersion::TapScript));
                SpendPath {
                    name,
                    script: tapscript,
                    control_block,
                }
            })
            .collect())
    }
}
