# Contract manager design sketch

This sketches a Rust-native contract manager inspired by the Go and TS SDKs, without copying their more stringly-typed contract model.

## Goal

Treat every watchable/spendable script as a persisted contract:

```text
script_pubkey -> stored contract -> registered handler -> spend/watch/discovery behavior
```

This should unify default VTXOs, delegate VTXOs, VHTLCs, boarding outputs, and user-provided custom scripts.

The `script_pubkey` is the canonical identity of a contract. Two stored contracts with the same script pubkey are considered the same contract, and stores should enforce uniqueness on this field. This also keeps the model compatible with other SDKs: the script is the shared identifier, not a Rust-specific contract ID.

## Core shape

Use strongly typed contract data, but persist through a type-erased envelope. Generic types should stay at API method boundaries; long-lived manager, wallet, and store structs should not be generic over contract type.

```rust
pub trait ContractSpec:
    Clone + serde::Serialize + for<'de> serde::Deserialize<'de> + Send + Sync + 'static
{
    const TYPE: ContractType;
    const VERSION: u32;

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error>;
    fn spendable_paths(&self, ctx: &SpendContext) -> Result<Vec<SpendPath>, Error>;
}
```

A typed contract value is useful at construction time, in tests, or when a caller knows the expected concrete type. It should not become the primary representation flowing through the whole SDK.

`SpendPath` should represent a path that is complete enough to spend through Taproot script path, not just a script fragment:

```rust
pub struct SpendPath {
    pub name: String,
    pub script: ScriptBuf,
    pub control_block: ControlBlock,
}
```

If a caller only needs script introspection/debugging, that should be modeled separately from `spendable_paths`, for example as a non-spendable `ContractPath` view. Keeping `control_block` required avoids treating script-only metadata as spend-ready.

The store keeps one common representation:

```rust
pub struct StoredContract {
    pub contract_type: ContractType,
    pub contract_version: u32,
    pub script_pubkey: ScriptBuf,
    pub state: ContractState,
    pub created_at: u64,
    pub key_index: Option<u32>,
    pub data: serde_json::Value,
}

pub enum ContractState {
    Active,
    Inactive,
}
```

`data` is serialized `T`. This keeps built-ins and custom contracts strongly typed at the API boundary while allowing one contract table / repository.

`state` is intentionally small and mirrors the broad TS SDK / Go SDK concept: it controls whether the contract is part of the manager's normal active set. It is not a VTXO lifecycle status. VTXO/output state remains the source of truth for whether value is pre-confirmed, confirmed, recoverable, spent, swept, exit-ready, etc.

- `Active`: include in normal watch/discovery/spend-reconstruction flows.
- `Inactive`: keep the contract metadata, but do not treat it as a current active receive/spend contract unless an explicit flow includes inactive contracts. This is useful for retired rotated receive contracts or stale custom contracts that may still need historical/debug handling.

`address` is intentionally not part of the canonical stored contract. It is derived from `script_pubkey + network`. The contract manager should be network-scoped, so stored contracts are never treated as portable between mainnet, signet, regtest, etc.

```rust
pub struct ContractManager {
    network: Network,
    registry: ContractRegistry,
    store: Box<dyn ContractStore>,
}
```

Debug/listing APIs can expose an address view without making address canonical persisted state:

```rust
pub struct ContractView {
    pub contract: StoredContract,
    pub address: Option<Address>,
}
```

## Contract type identifiers

Avoid raw strings in the public API. Contract type should be a small typed wrapper that serializes to a string:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContractType(String);
```

Built-ins should expose constants or constructors rather than asking users to type string names manually.

## Versioning

Stored contract data should include a version from the start:

```rust
pub struct StoredContract {
    pub contract_type: ContractType,
    pub contract_version: u32,
    pub script_pubkey: ScriptBuf,
    pub state: ContractState,
    pub created_at: u64,
    pub key_index: Option<u32>,
    pub data: serde_json::Value,
}
```

This matters when a contract payload evolves. For example, version 1 of a VHTLC might store only a SHA256 payment hash:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VhtlcContractV1 {
    pub sender_pubkey: XOnlyPublicKey,
    pub receiver_pubkey: XOnlyPublicKey,
    pub refund_delay: u16,
    pub payment_hash: [u8; 32],
}
```

A later version might support multiple hashlock kinds or additional expiry metadata:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VhtlcContractV2 {
    pub sender_pubkey: XOnlyPublicKey,
    pub receiver_pubkey: XOnlyPublicKey,
    pub refund_delay: u16,
    pub hashlock: Hashlock,
    pub absolute_expiry_height: Option<u32>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub enum Hashlock {
    Sha256([u8; 32]),
    Hash160([u8; 20]),
}
```

The handler can then explicitly support old records:

```rust
match stored.contract_version {
    1 => {
        let old: VhtlcContractV1 = serde_json::from_value(stored.data.clone())?;
        let current = VhtlcContractV2::from(old);
        current.spendable_paths(ctx)
    }
    2 => {
        let current: VhtlcContractV2 = serde_json::from_value(stored.data.clone())?;
        current.spendable_paths(ctx)
    }
    n => Err(Error::UnsupportedContractVersion(n)),
}
```

Without a stored version, deserialization and migration become ambiguous once payload formats change.

## Dynamic dispatch

The manager needs object-safe dispatch over many contract types. Use an erased handler adapter internally:

```rust
trait DynContractHandler: Send + Sync {
    fn contract_type(&self) -> ContractType;

    fn validate(&self, stored: &StoredContract, ctx: &ContractContext) -> Result<(), Error>;

    fn spendable_paths(
        &self,
        stored: &StoredContract,
        ctx: &SpendContext,
    ) -> Result<Vec<SpendPath>, Error>;
}
```

A generic adapter can implement `DynContractHandler` for any `T: ContractSpec` by deserializing `stored.data` into `T`, validating the derived script, then delegating behavior to `T`.

```rust
struct ContractHandler<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T: ContractSpec> DynContractHandler for ContractHandler<T> {
    fn contract_type(&self) -> ContractType {
        T::TYPE
    }

    fn validate(&self, stored: &StoredContract, ctx: &ContractContext) -> Result<(), Error> {
        if stored.contract_type != T::TYPE {
            return Err(Error::UnexpectedContractType);
        }
        if stored.contract_version != T::VERSION {
            return Err(Error::UnsupportedContractVersion(stored.contract_version));
        }

        let data: T = serde_json::from_value(stored.data.clone())?;
        let derived_script = data.script_pubkey(ctx)?;
        if derived_script != stored.script_pubkey {
            return Err(Error::ContractScriptMismatch);
        }

        Ok(())
    }

    fn spendable_paths(
        &self,
        stored: &StoredContract,
        ctx: &SpendContext,
    ) -> Result<Vec<SpendPath>, Error> {
        let data: T = serde_json::from_value(stored.data.clone())?;
        data.spendable_paths(ctx)
    }
}
```

Users should not normally implement or interact with `DynContractHandler` directly. Registration stays typed and concise:

```rust
manager.register::<DefaultContract>();
manager.register::<DelegateContract>();
manager.register::<VhtlcContract>();
manager.register::<MyCustomContract>();
```

This is the same broad pattern as the other SDKs: one handler per contract type, held in a registry. The Rust difference is that handlers can be backed by strongly typed data instead of `map[string]string` / `Record<string, string>` params.

## Typed and untyped loading

Typed loading is only appropriate when the caller already knows, or requires, the concrete contract type:

```rust
let default = manager.get_typed::<DefaultContract>(&script_pubkey)?;
```

That should be a checked convenience API, not the only way to load contracts. Generic SDK flows should be able to load and dispatch without knowing the type:

```rust
let stored = manager.get(&script_pubkey)?;
let paths = manager.spendable_paths_for_script(&script_pubkey, &spend_ctx)?;
```

Example shape:

```rust
impl ContractManager {
    pub fn get(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, Error> {
        self.store.get_by_script(script_pubkey)
    }

    pub fn spendable_paths_for_script(
        &self,
        script_pubkey: &Script,
        ctx: &SpendContext,
    ) -> Result<Vec<SpendPath>, Error> {
        let stored = self
            .store
            .get_by_script(script_pubkey)?
            .ok_or(Error::UnknownContractScript)?;

        let handler = self.registry.handler_for(&stored.contract_type)?;
        handler.spendable_paths(&stored, ctx)
    }

    pub fn get_typed<T: ContractSpec>(
        &self,
        script_pubkey: &Script,
    ) -> Result<Option<T>, Error> {
        let Some(stored) = self.store.get_by_script(script_pubkey)? else {
            return Ok(None);
        };

        if stored.contract_type != T::TYPE {
            return Err(Error::UnexpectedContractType);
        }

        let data = serde_json::from_value(stored.data)?;
        Ok(Some(data))
    }
}
```

So the design distinction is:

- unknown type: load `StoredContract`, then dispatch through the registered handler
- known type: use `get_typed::<T>()` as a checked convenience

## Manager responsibilities

Initial scope should be small:

- keep a network-scoped manager
- register handlers by contract type
- persist and list `StoredContract`s
- validate contracts derive their stored script
- derive address views from `script_pubkey + network` for debugging/listing
- dispatch complete spend path / tapscript reconstruction by script or contract type, including Taproot control blocks
- support gap-limit discovery once the basic model is in place

Avoid initially taking on the full TS-style repository sync/watcher design. That can be layered on later.

## Why not a large enum?

A built-in enum is ergonomic for SDK-owned contract types, but it makes user-provided custom contract types second-class. The `ContractSpec + StoredContract + DynContractHandler` shape gives us:

- strong typing for built-ins
- strong typing for custom contracts
- one persistent store
- one manager API
- dynamic extension through registered handlers

A built-in enum may still be useful as a convenience layer for SDK-owned contract types, but it should not be the core storage or dispatch model.

## Intended integration points

In the Rust SDK this should eventually replace ad-hoc reconstruction in places like:

- default/delegate address derivation
- key discovery
- VTXO script maps for listing/spending
- repeated VHTLC reconstruction in Boltz swap methods
- watcher subscription script selection

The important invariant is: if a VTXO has a script we care about, that script should resolve to a stored contract, and that contract's registered handler should provide the spend metadata.
