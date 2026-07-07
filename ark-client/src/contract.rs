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
use ark_core::contract::SpendSelection;
use ark_core::contract::StoredContract;
use ark_core::contract::VhtlcContract;
use ark_core::server;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkAddress;
use ark_core::BoardingOutput;
use ark_core::Vtxo;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::Script;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::marker::PhantomData;
#[cfg(feature = "sqlite")]
use std::path::Path;
#[cfg(feature = "sqlite")]
use std::sync::Mutex;
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
    fn spendable_selections(
        &self,
        stored: &StoredContract,
        ctx: &ContractContext,
    ) -> Result<Vec<SpendSelection>, Error>;
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

    fn spendable_selections(
        &self,
        stored: &StoredContract,
        ctx: &ContractContext,
    ) -> Result<Vec<SpendSelection>, Error> {
        self.validate(stored, ctx)?;
        let data: T = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode contract data: {e}")))?;
        data.spendable_selections(ctx).map_err(Into::into)
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

#[cfg(feature = "sqlite")]
pub struct SqliteContractStore {
    connection: Mutex<rusqlite::Connection>,
}

#[cfg(feature = "sqlite")]
impl SqliteContractStore {
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self, Error> {
        let db_path = db_path.as_ref();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::consumer(format!("failed to create contract store directory: {e}"))
            })?;
        }

        let connection = rusqlite::Connection::open(db_path)
            .map_err(|e| Error::consumer(format!("failed to open contract store: {e}")))?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn new_default() -> Result<Self, Error> {
        Self::new("contracts.db")
    }

    fn initialize(&self) -> Result<(), Error> {
        let connection = self.connection()?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS contracts (
                    script_pubkey BLOB PRIMARY KEY NOT NULL,
                    contract_type TEXT NOT NULL,
                    contract_version INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    key_index INTEGER,
                    data TEXT NOT NULL
                );",
            )
            .map_err(|e| Error::consumer(format!("failed to initialize contract store: {e}")))?;
        Ok(())
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>, Error> {
        self.connection
            .lock()
            .map_err(|_| Error::ad_hoc("contract store connection lock poisoned"))
    }

    fn state_to_str(state: ContractState) -> &'static str {
        match state {
            ContractState::Active => "active",
            ContractState::Inactive => "inactive",
        }
    }

    fn state_from_str(value: &str) -> Result<ContractState, Error> {
        match value {
            "active" => Ok(ContractState::Active),
            "inactive" => Ok(ContractState::Inactive),
            _ => Err(Error::ad_hoc(format!("unknown contract state: {value}"))),
        }
    }

    fn row_to_contract(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredContract> {
        let script_pubkey: Vec<u8> = row.get("script_pubkey")?;
        let contract_type: String = row.get("contract_type")?;
        let contract_version: i64 = row.get("contract_version")?;
        let state: String = row.get("state")?;
        let created_at: i64 = row.get("created_at")?;
        let key_index: Option<i64> = row.get("key_index")?;
        let data: String = row.get("data")?;

        let contract_type = ContractType::new(contract_type).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let state = Self::state_from_str(&state).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let data = serde_json::from_str(&data).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;

        Ok(StoredContract {
            contract_type,
            contract_version: u32::try_from(contract_version).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })?,
            script_pubkey: ScriptBuf::from_bytes(script_pubkey),
            state,
            created_at: u64::try_from(created_at).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })?,
            key_index: key_index
                .map(|value| {
                    u32::try_from(value).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Integer,
                            Box::new(e),
                        )
                    })
                })
                .transpose()?,
            data,
        })
    }
}

#[cfg(feature = "sqlite")]
impl ContractStore for SqliteContractStore {
    fn insert(&mut self, contract: StoredContract) -> Result<(), Error> {
        let data = serde_json::to_string(&contract.data)
            .map_err(|e| Error::ad_hoc(format!("failed to encode contract data: {e}")))?;
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO contracts (
                    script_pubkey,
                    contract_type,
                    contract_version,
                    state,
                    created_at,
                    key_index,
                    data
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    contract.script_pubkey.as_bytes(),
                    contract.contract_type.as_str(),
                    i64::from(contract.contract_version),
                    Self::state_to_str(contract.state),
                    i64::try_from(contract.created_at).map_err(|e| Error::ad_hoc(format!(
                        "contract created_at does not fit sqlite integer: {e}"
                    )))?,
                    contract.key_index.map(i64::from),
                    data,
                ],
            )
            .map_err(|e| {
                if matches!(e, rusqlite::Error::SqliteFailure(ref err, _) if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
                {
                    Error::ad_hoc("contract script already exists")
                } else {
                    Error::consumer(format!("failed to insert contract: {e}"))
                }
            })?;
        Ok(())
    }

    fn get_by_script(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, Error> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT script_pubkey, contract_type, contract_version, state, created_at, key_index, data
                 FROM contracts
                 WHERE script_pubkey = ?1",
            )
            .map_err(|e| Error::consumer(format!("failed to prepare contract lookup: {e}")))?;
        let mut rows = statement
            .query(rusqlite::params![script_pubkey.as_bytes()])
            .map_err(|e| Error::consumer(format!("failed to lookup contract: {e}")))?;
        let Some(row) = rows
            .next()
            .map_err(|e| Error::consumer(format!("failed to read contract: {e}")))?
        else {
            return Ok(None);
        };
        Self::row_to_contract(row)
            .map(Some)
            .map_err(|e| Error::consumer(format!("failed to decode contract: {e}")))
    }

    fn list(&self) -> Result<Vec<StoredContract>, Error> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT script_pubkey, contract_type, contract_version, state, created_at, key_index, data
                 FROM contracts
                 ORDER BY created_at, rowid",
            )
            .map_err(|e| Error::consumer(format!("failed to prepare contract list: {e}")))?;
        let rows = statement
            .query_map([], Self::row_to_contract)
            .map_err(|e| Error::consumer(format!("failed to list contracts: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::consumer(format!("failed to decode contracts: {e}")))
    }

    fn update_state(&mut self, script_pubkey: &Script, state: ContractState) -> Result<(), Error> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE contracts SET state = ?1 WHERE script_pubkey = ?2",
                rusqlite::params![Self::state_to_str(state), script_pubkey.as_bytes()],
            )
            .map_err(|e| Error::consumer(format!("failed to update contract state: {e}")))?;
        if updated == 0 {
            return Err(Error::ad_hoc("unknown contract script"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContractVtxo {
    pub contract: StoredContract,
    pub vtxo: VirtualTxOutPoint,
    pub spend_selections: Vec<SpendSelection>,
}

impl ContractVtxo {
    pub fn spend_path(&self, kind: ark_core::contract::SpendPathKind) -> Result<SpendPath, Error> {
        self.spend_selection(kind).map(|selection| selection.path)
    }

    pub fn spend_selection(
        &self,
        kind: ark_core::contract::SpendPathKind,
    ) -> Result<SpendSelection, Error> {
        self.spend_selections
            .iter()
            .find(|selection| selection.path.kind == kind)
            .cloned()
            .ok_or_else(|| Error::ad_hoc(format!("missing {kind:?} spend path")))
    }

    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        self.spend_selections
            .iter()
            .map(|selection| selection.path.script.clone())
            .collect()
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.contract.script_pubkey.clone()
    }

    pub fn server_pk(&self) -> Result<XOnlyPublicKey, Error> {
        Ok(self.vtxo_contract_data()?.server)
    }

    pub fn owner_pk(&self) -> Result<XOnlyPublicKey, Error> {
        Ok(self.vtxo_contract_data()?.owner)
    }

    pub fn exit_delay(&self) -> Result<Sequence, Error> {
        Ok(self.vtxo_contract_data()?.exit_delay)
    }

    fn vtxo_contract_data(&self) -> Result<VtxoContractData, Error> {
        offchain_vtxo_data(&self.contract)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VtxoContractData {
    server: XOnlyPublicKey,
    owner: XOnlyPublicKey,
    exit_delay: Sequence,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContractBoardingOutput {
    pub contract: StoredContract,
    pub output: BoardingOutput,
    pub spend_selections: Vec<SpendSelection>,
}

impl ContractBoardingOutput {
    pub fn spend_path(&self, kind: ark_core::contract::SpendPathKind) -> Result<SpendPath, Error> {
        self.spend_selection(kind).map(|selection| selection.path)
    }

    pub fn spend_selection(
        &self,
        kind: ark_core::contract::SpendPathKind,
    ) -> Result<SpendSelection, Error> {
        self.spend_selections
            .iter()
            .find(|selection| selection.path.kind == kind)
            .cloned()
            .ok_or_else(|| Error::ad_hoc(format!("missing {kind:?} spend path")))
    }

    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        self.spend_selections
            .iter()
            .map(|selection| selection.path.script.clone())
            .collect()
    }

    pub fn address(&self) -> &Address {
        self.output.address()
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.contract.script_pubkey.clone()
    }

    pub fn server_pk(&self) -> XOnlyPublicKey {
        self.output.server_pk()
    }

    pub fn owner_pk(&self) -> XOnlyPublicKey {
        self.output.owner_pk()
    }

    pub fn exit_delay(&self) -> Sequence {
        self.output.exit_delay()
    }

    pub fn can_be_claimed_unilaterally_by_owner(
        &self,
        now: std::time::Duration,
        confirmation_blocktime: std::time::Duration,
        confirmations: u64,
    ) -> bool {
        self.output
            .can_be_claimed_unilaterally_by_owner(now, confirmation_blocktime, confirmations)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveOffchainContract {
    pub address: ArkAddress,
    pub vtxo: Vtxo,
    pub spend_selections: Vec<SpendSelection>,
}

impl ActiveOffchainContract {
    pub fn spend_selection(
        &self,
        kind: ark_core::contract::SpendPathKind,
    ) -> Result<SpendSelection, Error> {
        self.spend_selections
            .iter()
            .find(|selection| selection.path.kind == kind)
            .cloned()
            .ok_or_else(|| Error::ad_hoc(format!("missing {kind:?} spend path")))
    }
}

#[derive(Clone, Debug)]
pub struct ContractVtxoList {
    dust: Amount,
    vtxos: Vec<ContractVtxo>,
}

impl ContractVtxoList {
    pub fn new(dust: Amount, vtxos: Vec<ContractVtxo>) -> Self {
        Self { dust, vtxos }
    }

    pub fn into_inner(self) -> Vec<ContractVtxo> {
        self.vtxos
    }

    pub fn all(&self) -> impl Iterator<Item = &ContractVtxo> {
        self.vtxos.iter()
    }

    pub fn all_unspent(&self) -> impl Iterator<Item = &ContractVtxo> {
        let dust = self.dust;
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_unspent(dust))
    }

    pub fn spendable_offchain(&self) -> impl Iterator<Item = &ContractVtxo> {
        let dust = self.dust;
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_spendable_offchain(dust))
    }

    pub fn spendable_offchain_at<'a>(
        &'a self,
        server_info: &'a server::Info,
        now_unix_secs: i64,
    ) -> impl Iterator<Item = &'a ContractVtxo> + 'a {
        self.spendable_offchain().filter(move |entry| {
            !entry
                .server_pk()
                .map(|server_pk| server_info.signer_requires_recovery_at(server_pk, now_unix_secs))
                .unwrap_or(false)
        })
    }

    pub fn pending_recovery_due_to_signer_at<'a>(
        &'a self,
        server_info: &'a server::Info,
        now_unix_secs: i64,
    ) -> impl Iterator<Item = &'a ContractVtxo> + 'a {
        self.spendable_offchain().filter(move |entry| {
            entry
                .server_pk()
                .map(|server_pk| server_info.signer_requires_recovery_at(server_pk, now_unix_secs))
                .unwrap_or(false)
        })
    }

    pub fn batch_settleable_at<'a>(
        &'a self,
        server_info: &'a server::Info,
        now_unix_secs: i64,
    ) -> impl Iterator<Item = &'a ContractVtxo> + 'a {
        self.all_unspent().filter(move |entry| {
            entry.vtxo.is_recoverable(server_info.dust)
                || !entry
                    .server_pk()
                    .map(|server_pk| {
                        server_info.signer_requires_recovery_at(server_pk, now_unix_secs)
                    })
                    .unwrap_or(false)
        })
    }

    pub fn pre_confirmed(&self) -> impl Iterator<Item = &ContractVtxo> {
        let dust = self.dust;
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_pre_confirmed_spendable(dust))
    }

    pub fn confirmed(&self) -> impl Iterator<Item = &ContractVtxo> {
        let dust = self.dust;
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_confirmed_spendable(dust))
    }

    pub fn recoverable(&self) -> impl Iterator<Item = &ContractVtxo> {
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_recoverable(self.dust))
    }

    pub fn could_exit_unilaterally(&self) -> impl Iterator<Item = &ContractVtxo> {
        self.pre_confirmed().chain(self.confirmed())
    }

    pub fn spent(&self) -> impl Iterator<Item = &ContractVtxo> {
        let dust = self.dust;
        self.vtxos
            .iter()
            .filter(move |entry| entry.vtxo.is_spent_status(dust))
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

        match self.store.get_by_script(&stored.script_pubkey)? {
            None => {
                self.store.insert(stored.clone())?;
                Ok(stored)
            }
            Some(existing) if same_stored_contract(&existing, &stored) => Ok(existing),
            Some(existing) if can_share_script_row(&existing, &stored)? => Ok(existing),
            Some(_) => Err(Error::ad_hoc(
                "contract script already exists with different data",
            )),
        }
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

    pub fn spendable_selections(
        &self,
        stored: &StoredContract,
    ) -> Result<Vec<SpendSelection>, Error> {
        let ctx = ContractContext::new(self.network);
        let handler = self.registry.handler_for(&stored.contract_type)?;
        handler.spendable_selections(stored, &ctx)
    }

    pub(crate) fn active_offchain_contracts(
        &self,
        unilateral_exit_delay_candidates: &[Sequence],
    ) -> Result<Vec<ActiveOffchainContract>, Error> {
        let ctx = ContractContext::new(self.network);
        self.store
            .list()?
            .into_iter()
            .filter(|stored| stored.state == ContractState::Active)
            .filter_map(|stored| {
                match active_offchain_contract_from_stored(
                    self,
                    &ctx,
                    stored,
                    unilateral_exit_delay_candidates,
                ) {
                    Ok(Some(contract)) => Some(Ok(contract)),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .collect()
    }

    pub fn annotate_vtxos(
        &self,
        vtxos: Vec<VirtualTxOutPoint>,
    ) -> Result<Vec<ContractVtxo>, Error> {
        vtxos
            .into_iter()
            .map(|vtxo| {
                let contract = self
                    .store
                    .get_by_script(&vtxo.script)?
                    .ok_or_else(|| Error::ad_hoc("unknown contract script"))?;
                let spend_selections = self.spendable_selections(&contract)?;
                Ok(ContractVtxo {
                    contract,
                    vtxo,
                    spend_selections,
                })
            })
            .collect()
    }

    /// Return active boarding outputs, including compatible default VTXO rows.
    ///
    /// The store keeps one row per script. If a default VTXO row was stored before an equivalent
    /// boarding row, on-chain boarding discovery must still see it as a boarding output. Default
    /// VTXO rows are included only when their CSV delay is one of the caller's boarding delay
    /// candidates. Passing an empty slice means "strict boarding rows only".
    pub fn annotated_boarding_outputs_for_exit_delays(
        &self,
        compatible_default_exit_delays: &[Sequence],
    ) -> Result<Vec<ContractBoardingOutput>, Error> {
        let ctx = ContractContext::new(self.network);
        self.store
            .list()?
            .into_iter()
            .filter(|stored| stored.state == ContractState::Active)
            .filter_map(|stored| {
                boarding_contract_from_stored(&stored, compatible_default_exit_delays)
                    .map(|contract| (stored, contract))
            })
            .map(|(stored, contract)| {
                let output = contract.boarding_output(&ctx)?;
                let spend_selections = self.spendable_selections(&stored)?;
                Ok(ContractBoardingOutput {
                    contract: stored,
                    output,
                    spend_selections,
                })
            })
            .collect()
    }

    pub fn annotated_boarding_outputs(&self) -> Result<Vec<ContractBoardingOutput>, Error> {
        self.annotated_boarding_outputs_for_exit_delays(&[])
    }
}

fn same_stored_contract(a: &StoredContract, b: &StoredContract) -> bool {
    a.contract_type == b.contract_type
        && a.contract_version == b.contract_version
        && a.data == b.data
}

/// Whether two same-script rows may use the row that was stored first.
///
/// This is intentionally limited to default VTXO/boarding rows that decode to the same two-leaf
/// server+owner/CSV template. Delegate and VHTLC scripts carry different leaves/semantics and a
/// same-script collision with them should remain a hard error.
fn can_share_script_row(a: &StoredContract, b: &StoredContract) -> Result<bool, Error> {
    let default_vtxo_boarding = a.contract_type == ContractType::default_vtxo()
        && b.contract_type == ContractType::boarding();
    let boarding_default_vtxo = a.contract_type == ContractType::boarding()
        && b.contract_type == ContractType::default_vtxo();
    if !default_vtxo_boarding && !boarding_default_vtxo {
        return Ok(false);
    }

    // Store only one row for a script. Allow default VTXO and boarding to share that row only
    // when the decoded script template is identical.
    Ok(two_leaf_vtxo_data(a)? == two_leaf_vtxo_data(b)?)
}

fn active_offchain_contract_from_stored(
    manager: &ContractManager,
    ctx: &ContractContext,
    stored: StoredContract,
    unilateral_exit_delay_candidates: &[Sequence],
) -> Result<Option<ActiveOffchainContract>, Error> {
    if stored.contract_type == ContractType::delegate_vtxo() {
        let contract: DelegateVtxoContract = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode delegate vtxo contract: {e}")))?;
        return Ok(Some(active_offchain_contract(
            manager,
            &stored,
            contract.vtxo(ctx)?,
        )?));
    }

    let data = match two_leaf_vtxo_data(&stored) {
        Ok(data) => data,
        Err(_) => return Ok(None),
    };

    // A boarding row can also represent an offchain default VTXO row for the same script, but only
    // when its CSV delay is one of the delays used for unilateral-exit VTXOs. Other boarding rows
    // must not be queried as Arkade receive addresses.
    if stored.contract_type == ContractType::boarding()
        && !unilateral_exit_delay_candidates.contains(&data.exit_delay)
    {
        return Ok(None);
    }

    let contract = DefaultVtxoContract {
        server: data.server,
        owner: data.owner,
        exit_delay: data.exit_delay,
    };
    Ok(Some(active_offchain_contract(
        manager,
        &stored,
        contract.vtxo(ctx)?,
    )?))
}

fn active_offchain_contract(
    manager: &ContractManager,
    stored: &StoredContract,
    vtxo: Vtxo,
) -> Result<ActiveOffchainContract, Error> {
    Ok(ActiveOffchainContract {
        address: vtxo.to_ark_address(),
        vtxo,
        spend_selections: manager.spendable_selections(stored)?,
    })
}

fn offchain_vtxo_data(stored: &StoredContract) -> Result<VtxoContractData, Error> {
    two_leaf_vtxo_data(stored).or_else(|_| delegate_vtxo_data(stored))
}

fn delegate_vtxo_data(stored: &StoredContract) -> Result<VtxoContractData, Error> {
    if stored.contract_type != ContractType::delegate_vtxo() {
        return Err(Error::ad_hoc(format!(
            "contract type {} is not a delegate vtxo contract",
            stored.contract_type
        )));
    }
    let contract: DelegateVtxoContract = serde_json::from_value(stored.data.clone())
        .map_err(|e| Error::ad_hoc(format!("failed to decode delegate vtxo contract: {e}")))?;
    Ok(VtxoContractData {
        server: contract.server,
        owner: contract.owner,
        exit_delay: contract.exit_delay,
    })
}

/// Decode rows that use the shared two-leaf default VTXO/boarding template.
///
/// Both contract types produce the same spend paths when server, owner and CSV delay match. This
/// helper is the single place that treats them as the same template; callers decide whether that
/// template is being used as an offchain VTXO or as an on-chain boarding output.
fn two_leaf_vtxo_data(stored: &StoredContract) -> Result<VtxoContractData, Error> {
    if stored.contract_type == ContractType::default_vtxo() {
        let contract: DefaultVtxoContract = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode default vtxo contract: {e}")))?;
        return Ok(VtxoContractData {
            server: contract.server,
            owner: contract.owner,
            exit_delay: contract.exit_delay,
        });
    }
    if stored.contract_type == ContractType::boarding() {
        let contract: BoardingContract = serde_json::from_value(stored.data.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to decode boarding contract: {e}")))?;
        return Ok(VtxoContractData {
            server: contract.server,
            owner: contract.owner,
            exit_delay: contract.exit_delay,
        });
    }
    Err(Error::ad_hoc(format!(
        "contract type {} is not a two-leaf vtxo contract",
        stored.contract_type
    )))
}

/// Resolve a stored row into boarding semantics when safe.
///
/// Real boarding rows always qualify. Default VTXO rows qualify only as a script-sharing fallback
/// and only for the boarding exit-delay candidates supplied by the caller; otherwise every default
/// VTXO row would incorrectly appear as an on-chain boarding address.
fn boarding_contract_from_stored(
    stored: &StoredContract,
    compatible_default_vtxo_exit_delays: &[Sequence],
) -> Option<BoardingContract> {
    let data = two_leaf_vtxo_data(stored).ok()?;

    // A default VTXO row can also represent a boarding row for the same script, but only when the
    // caller is explicitly watching that CSV delay as a boarding delay.
    if stored.contract_type == ContractType::boarding()
        || compatible_default_vtxo_exit_delays.contains(&data.exit_delay)
    {
        return Some(BoardingContract {
            server: data.server,
            owner: data.owner,
            exit_delay: data.exit_delay,
        });
    }

    None
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
    use ark_core::contract::SpendPathKind;
    use bitcoin::Amount;
    use bitcoin::OutPoint;
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
    fn annotates_vtxos_with_contract_spend_paths() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();

        let contract = DefaultVtxoContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let stored = manager
            .insert(contract, ContractState::Active, Some(7))
            .unwrap();
        let vtxo = VirtualTxOutPoint {
            outpoint: OutPoint::null(),
            created_at: 0,
            expires_at: 0,
            amount: Amount::from_sat(42_000),
            script: stored.script_pubkey.clone(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
        };

        let annotated = manager.annotate_vtxos(vec![vtxo.clone()]).unwrap();

        assert_eq!(annotated.len(), 1);
        assert_eq!(annotated[0].contract, stored);
        assert_eq!(annotated[0].vtxo, vtxo);
        assert_eq!(annotated[0].spend_selections.len(), 2);
    }

    #[test]
    fn annotates_boarding_outputs_with_contract_spend_paths() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();

        let contract = BoardingContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let stored = manager
            .insert(contract, ContractState::Active, Some(7))
            .unwrap();

        let annotated = manager.annotated_boarding_outputs().unwrap();

        assert_eq!(annotated.len(), 1);
        assert_eq!(annotated[0].contract, stored);
        assert_eq!(annotated[0].script_pubkey(), stored.script_pubkey);
        assert_eq!(annotated[0].server_pk(), server);
        assert_eq!(annotated[0].owner_pk(), owner);
        assert_eq!(annotated[0].spend_selections.len(), 2);
        assert!(annotated[0].spend_selection(SpendPathKind::Forfeit).is_ok());
        assert!(annotated[0].spend_selection(SpendPathKind::Exit).is_ok());
    }

    #[test]
    fn default_vtxo_and_boarding_can_share_script_row() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();
        let exit_delay = Sequence::from_seconds_ceil(86400).unwrap();

        let default = DefaultVtxoContract {
            server,
            owner,
            exit_delay,
        };
        let boarding = BoardingContract {
            server,
            owner,
            exit_delay,
        };

        let stored_default = manager
            .insert_or_get(default, ContractState::Active, Some(7))
            .unwrap();
        let stored_boarding = manager
            .insert_or_get(boarding, ContractState::Active, Some(7))
            .unwrap();

        assert_eq!(stored_boarding, stored_default);
        assert_eq!(stored_default.contract_type, ContractType::default_vtxo());
        assert_eq!(manager.list().unwrap().len(), 1);

        let boarding_outputs = manager
            .annotated_boarding_outputs_for_exit_delays(&[exit_delay])
            .unwrap();
        assert_eq!(boarding_outputs.len(), 1);
        assert_eq!(boarding_outputs[0].contract, stored_default);
        assert_eq!(boarding_outputs[0].server_pk(), server);
        assert_eq!(boarding_outputs[0].owner_pk(), owner);
    }

    #[test]
    fn boarding_contract_can_annotate_offchain_vtxo() {
        let (server, owner, _) = test_keys();
        let mut manager = ContractManager::in_memory(Network::Regtest);
        manager.register_builtins().unwrap();
        let exit_delay = Sequence::from_seconds_ceil(86400).unwrap();

        let stored = manager
            .insert_or_get(
                BoardingContract {
                    server,
                    owner,
                    exit_delay,
                },
                ContractState::Active,
                Some(7),
            )
            .unwrap();
        let vtxo = VirtualTxOutPoint {
            outpoint: OutPoint::null(),
            created_at: 0,
            expires_at: 0,
            amount: Amount::from_sat(42_000),
            script: stored.script_pubkey.clone(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
        };

        let annotated = manager.annotate_vtxos(vec![vtxo]).unwrap();

        assert_eq!(annotated.len(), 1);
        assert_eq!(annotated[0].contract, stored);
        assert_eq!(annotated[0].server_pk().unwrap(), server);
        assert_eq!(annotated[0].owner_pk().unwrap(), owner);
        assert_eq!(annotated[0].exit_delay().unwrap(), exit_delay);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_store_persists_contracts() {
        let (server, owner, _) = test_keys();
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("contracts.db");
        let mut manager = ContractManager::new(
            Network::Regtest,
            Box::new(SqliteContractStore::new(&db_path).unwrap()),
        );
        manager.register_builtins().unwrap();

        let contract = DefaultVtxoContract {
            server,
            owner,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let stored = manager
            .insert(contract, ContractState::Active, Some(7))
            .unwrap();
        manager
            .update_state(&stored.script_pubkey, ContractState::Inactive)
            .unwrap();

        let mut reopened = ContractManager::new(
            Network::Regtest,
            Box::new(SqliteContractStore::new(&db_path).unwrap()),
        );
        reopened.register_builtins().unwrap();

        let persisted = reopened.get(&stored.script_pubkey).unwrap().unwrap();
        assert_eq!(persisted.state, ContractState::Inactive);
        assert_eq!(persisted.contract_type, ContractType::default_vtxo());
        assert_eq!(persisted.key_index, Some(7));
        assert_eq!(persisted.data, stored.data);
        assert_eq!(reopened.list().unwrap().len(), 1);
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
