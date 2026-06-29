use crate::Error;
use ark_core::contract::BoardingContract;
use ark_core::contract::ContractContext;
use ark_core::contract::ContractSpec;
use ark_core::contract::ContractState;
use ark_core::contract::ContractType;
use ark_core::contract::ContractView;
use ark_core::contract::DefaultVtxoContract;
use ark_core::contract::DelegateVtxoContract;
use ark_core::contract::SpendPath;
use ark_core::contract::StoredContract;
use ark_core::contract::VhtlcContract;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::Script;
use bitcoin::ScriptBuf;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

trait DynContractHandler: Send + Sync {
    fn contract_type(&self) -> ContractType;
    fn validate(&self, stored: &StoredContract, ctx: &ContractContext) -> Result<(), Error>;
    fn spendable_paths(
        &self,
        stored: &StoredContract,
        ctx: &ContractContext,
    ) -> Result<Vec<SpendPath>, Error>;
}

struct ContractHandler<T> {
    _marker: PhantomData<T>,
}

impl<T> Default for ContractHandler<T> {
    fn default() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<T: ContractSpec> DynContractHandler for ContractHandler<T> {
    fn contract_type(&self) -> ContractType {
        T::contract_type()
    }

    fn validate(&self, stored: &StoredContract, ctx: &ContractContext) -> Result<(), Error> {
        if stored.contract_type != T::contract_type() {
            return Err(Error::ad_hoc("unexpected contract type"));
        }
        if stored.contract_version != T::VERSION {
            return Err(Error::ad_hoc(format!(
                "unsupported contract version: {}",
                stored.contract_version
            )));
        }

        let data: T = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode contract data: {e}")))?;
        let derived_script = data.script_pubkey(ctx)?;
        if derived_script != stored.script_pubkey {
            return Err(Error::ad_hoc("contract script mismatch"));
        }

        Ok(())
    }

    fn spendable_paths(
        &self,
        stored: &StoredContract,
        ctx: &ContractContext,
    ) -> Result<Vec<SpendPath>, Error> {
        self.validate(stored, ctx)?;
        let data: T = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode contract data: {e}")))?;
        data.spendable_paths(ctx).map_err(Into::into)
    }
}

#[derive(Default)]
pub struct ContractRegistry {
    handlers: HashMap<ContractType, Box<dyn DynContractHandler>>,
}

impl ContractRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: ContractSpec>(&mut self) -> Result<(), Error> {
        let contract_type = T::contract_type();
        if self.handlers.contains_key(&contract_type) {
            return Err(Error::ad_hoc(format!(
                "contract handler already registered: {contract_type}"
            )));
        }
        let handler = Box::new(ContractHandler::<T>::default());
        debug_assert_eq!(handler.contract_type(), contract_type);
        self.handlers.insert(contract_type, handler);
        Ok(())
    }

    fn handler_for(&self, contract_type: &ContractType) -> Result<&dyn DynContractHandler, Error> {
        self.handlers
            .get(contract_type)
            .map(|handler| handler.as_ref())
            .ok_or_else(|| Error::ad_hoc(format!("unknown contract type: {contract_type}")))
    }
}

pub trait ContractStore: Send + Sync {
    fn insert(&mut self, contract: StoredContract) -> Result<(), Error>;
    fn get_by_script(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, Error>;
    fn list(&self) -> Result<Vec<StoredContract>, Error>;
    fn update_state(&mut self, script_pubkey: &Script, state: ContractState) -> Result<(), Error>;
}

#[derive(Default)]
pub struct MemoryContractStore {
    contracts: HashMap<ScriptBuf, StoredContract>,
}

impl MemoryContractStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContractStore for MemoryContractStore {
    fn insert(&mut self, contract: StoredContract) -> Result<(), Error> {
        if self.contracts.contains_key(&contract.script_pubkey) {
            return Err(Error::ad_hoc("contract script already exists"));
        }
        self.contracts
            .insert(contract.script_pubkey.clone(), contract);
        Ok(())
    }

    fn get_by_script(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, Error> {
        Ok(self.contracts.get(script_pubkey).cloned())
    }

    fn list(&self) -> Result<Vec<StoredContract>, Error> {
        Ok(self.contracts.values().cloned().collect())
    }

    fn update_state(&mut self, script_pubkey: &Script, state: ContractState) -> Result<(), Error> {
        let contract = self
            .contracts
            .get_mut(script_pubkey)
            .ok_or_else(|| Error::ad_hoc("unknown contract script"))?;
        contract.state = state;
        Ok(())
    }
}

pub struct ContractManager {
    network: Network,
    registry: ContractRegistry,
    store: Box<dyn ContractStore>,
}

impl ContractManager {
    pub fn new(network: Network, store: Box<dyn ContractStore>) -> Self {
        Self {
            network,
            registry: ContractRegistry::new(),
            store,
        }
    }

    pub fn in_memory(network: Network) -> Self {
        Self::new(network, Box::new(MemoryContractStore::new()))
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn register<T: ContractSpec>(&mut self) -> Result<(), Error> {
        self.registry.register::<T>()
    }

    pub fn register_builtins(&mut self) -> Result<(), Error> {
        self.register::<DefaultVtxoContract>()?;
        self.register::<DelegateVtxoContract>()?;
        self.register::<BoardingContract>()?;
        self.register::<VhtlcContract>()
    }

    pub fn insert<T: ContractSpec>(
        &mut self,
        contract: T,
        state: ContractState,
        key_index: Option<u32>,
    ) -> Result<StoredContract, Error> {
        let stored = self.stored_contract(contract, state, key_index)?;
        self.store.insert(stored.clone())?;
        Ok(stored)
    }

    pub fn insert_or_get<T: ContractSpec>(
        &mut self,
        contract: T,
        state: ContractState,
        key_index: Option<u32>,
    ) -> Result<StoredContract, Error> {
        let stored = self.stored_contract(contract, state, key_index)?;
        if let Some(existing) = self.store.get_by_script(&stored.script_pubkey)? {
            if existing.contract_type != stored.contract_type
                || existing.contract_version != stored.contract_version
                || existing.data != stored.data
            {
                return Err(Error::ad_hoc(
                    "contract script already exists with different data",
                ));
            }
            return Ok(existing);
        }
        self.store.insert(stored.clone())?;
        Ok(stored)
    }

    fn stored_contract<T: ContractSpec>(
        &self,
        contract: T,
        state: ContractState,
        key_index: Option<u32>,
    ) -> Result<StoredContract, Error> {
        let ctx = ContractContext::new(self.network);
        let stored = StoredContract {
            contract_type: T::contract_type(),
            contract_version: T::VERSION,
            script_pubkey: contract.script_pubkey(&ctx)?,
            state,
            created_at: now_secs()?,
            key_index,
            data: serde_json::to_value(contract)
                .map_err(|e| Error::ad_hoc(format!("failed to encode contract data: {e}")))?,
        };

        let handler = self.registry.handler_for(&stored.contract_type)?;
        handler.validate(&stored, &ctx)?;
        Ok(stored)
    }

    pub fn insert_stored(&mut self, stored: StoredContract) -> Result<(), Error> {
        let ctx = ContractContext::new(self.network);
        let handler = self.registry.handler_for(&stored.contract_type)?;
        handler.validate(&stored, &ctx)?;
        self.store.insert(stored)
    }

    pub fn get(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, Error> {
        self.store.get_by_script(script_pubkey)
    }

    pub fn get_typed<T: ContractSpec>(&self, script_pubkey: &Script) -> Result<Option<T>, Error> {
        let Some(stored) = self.store.get_by_script(script_pubkey)? else {
            return Ok(None);
        };

        if stored.contract_type != T::contract_type() {
            return Err(Error::ad_hoc("unexpected contract type"));
        }
        if stored.contract_version != T::VERSION {
            return Err(Error::ad_hoc(format!(
                "unsupported contract version: {}",
                stored.contract_version
            )));
        }

        serde_json::from_value(stored.data)
            .map(Some)
            .map_err(|e| Error::ad_hoc(format!("failed to decode contract data: {e}")))
    }

    pub fn list(&self) -> Result<Vec<StoredContract>, Error> {
        self.store.list()
    }

    pub fn list_by_type(&self, contract_type: ContractType) -> Result<Vec<StoredContract>, Error> {
        Ok(self
            .store
            .list()?
            .into_iter()
            .filter(|contract| contract.contract_type == contract_type)
            .collect())
    }

    pub fn list_active_by_type(
        &self,
        contract_type: ContractType,
    ) -> Result<Vec<StoredContract>, Error> {
        Ok(self
            .list_by_type(contract_type)?
            .into_iter()
            .filter(|contract| contract.state == ContractState::Active)
            .collect())
    }

    pub fn list_views(&self) -> Result<Vec<ContractView>, Error> {
        self.store
            .list()?
            .into_iter()
            .map(|contract| {
                let address = Address::from_script(&contract.script_pubkey, self.network).ok();
                Ok(ContractView { contract, address })
            })
            .collect()
    }

    pub fn update_state(
        &mut self,
        script_pubkey: &Script,
        state: ContractState,
    ) -> Result<(), Error> {
        self.store.update_state(script_pubkey, state)
    }

    pub fn spendable_paths_for_script(
        &self,
        script_pubkey: &Script,
    ) -> Result<Vec<SpendPath>, Error> {
        let stored = self
            .store
            .get_by_script(script_pubkey)?
            .ok_or_else(|| Error::ad_hoc("unknown contract script"))?;
        self.spendable_paths(&stored)
    }

    pub fn spendable_paths(&self, stored: &StoredContract) -> Result<Vec<SpendPath>, Error> {
        let ctx = ContractContext::new(self.network);
        let handler = self.registry.handler_for(&stored.contract_type)?;
        handler.spendable_paths(stored, &ctx)
    }
}

fn now_secs() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| Error::ad_hoc(format!("system clock before unix epoch: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Sequence;
    use bitcoin::XOnlyPublicKey;
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
    fn stores_and_dispatches_default_contract() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();

        let contract = DefaultVtxoContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let stored = manager
            .insert(contract.clone(), ContractState::Active, Some(7))
            .unwrap();

        assert_eq!(stored.contract_type, ContractType::default_vtxo());
        assert_eq!(stored.key_index, Some(7));
        assert_eq!(
            manager.get(&stored.script_pubkey).unwrap(),
            Some(stored.clone())
        );
        assert_eq!(
            manager
                .get_typed::<DefaultVtxoContract>(&stored.script_pubkey)
                .unwrap(),
            Some(contract)
        );

        let paths = manager
            .spendable_paths_for_script(&stored.script_pubkey)
            .unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|path| !path.script.is_empty()));
    }

    #[test]
    fn store_enforces_script_uniqueness() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();
        let contract = DefaultVtxoContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };

        manager
            .insert(contract.clone(), ContractState::Active, None)
            .unwrap();
        assert!(manager
            .insert(contract, ContractState::Active, None)
            .is_err());
    }

    #[test]
    fn validates_script_mismatch() {
        let (server, owner, delegator) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();
        let default = DefaultVtxoContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let delegate = DelegateVtxoContract {
            server,
            owner,
            delegator,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let ctx = ContractContext::new(Network::Regtest);
        let stored = StoredContract {
            contract_type: ContractType::default_vtxo(),
            contract_version: DefaultVtxoContract::VERSION,
            script_pubkey: delegate.script_pubkey(&ctx).unwrap(),
            state: ContractState::Active,
            created_at: 0,
            key_index: None,
            data: serde_json::to_value(default).unwrap(),
        };

        assert!(manager.insert_stored(stored).is_err());
    }
}
