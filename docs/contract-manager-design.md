# Contract manager design sketch

This sketches a Rust-native contract manager inspired by the Go and TS SDKs, without copying their more stringly-typed contract model.

## Goal

Treat every watchable/spendable script as a persisted contract:

```text
script_pubkey -> stored contract -> registered handler -> spend/watch/discovery behavior
```

This should unify default VTXOs, delegate VTXOs, VHTLCs, boarding outputs, and user-provided custom scripts.

## Core shape

Use strongly typed contract data, but persist through a type-erased envelope.

```rust
pub trait ContractSpec:
    Clone + serde::Serialize + for<'de> serde::Deserialize<'de> + Send + Sync + 'static
{
    const TYPE: &'static str;

    fn script_pubkey(&self, ctx: &ContractContext) -> Result<ScriptBuf, Error>;
    fn address(&self, ctx: &ContractContext) -> Result<ContractAddress, Error>;
    fn spendable_paths(&self, ctx: &SpendContext) -> Result<Vec<SpendPath>, Error>;
}
```

A typed contract is useful when the caller knows the concrete type:

```rust
pub struct Contract<T: ContractSpec> {
    pub script_pubkey: ScriptBuf,
    pub address: ContractAddress,
    pub state: ContractState,
    pub created_at: u64,
    pub key_index: Option<u32>,
    pub data: T,
}
```

The store keeps one common representation:

```rust
pub struct StoredContract {
    pub contract_type: String,
    pub script_pubkey: ScriptBuf,
    pub address: ContractAddress,
    pub state: ContractState,
    pub created_at: u64,
    pub key_index: Option<u32>,
    pub data: serde_json::Value,
}
```

`data` is serialized `T`. This keeps built-ins and custom contracts strongly typed at the API boundary while allowing one contract table / repository.

## Dynamic dispatch

The manager needs object-safe dispatch over many contract types. Use an erased handler adapter:

```rust
pub trait DynContractHandler: Send + Sync {
    fn contract_type(&self) -> &'static str;
    fn validate(&self, stored: &StoredContract, ctx: &ContractContext) -> Result<(), Error>;
    fn spendable_paths(
        &self,
        stored: &StoredContract,
        ctx: &SpendContext,
    ) -> Result<Vec<SpendPath>, Error>;
}
```

A generic adapter can implement `DynContractHandler` for any `T: ContractSpec` by deserializing `stored.data` into `T`, validating the derived script, then delegating behavior to `T`.

Registration then looks like:

```rust
manager.register::<DefaultContract>();
manager.register::<DelegateContract>();
manager.register::<VhtlcContract>();
manager.register::<MyCustomContract>();
```

This is the same broad pattern as the other SDKs: one handler per contract type, held in a registry. The Rust difference is that handlers can be backed by strongly typed data instead of `map[string]string` / `Record<string, string>` params.

## Manager responsibilities

Initial scope should be small:

- register handlers by contract type
- persist and list `StoredContract`s
- validate contracts derive their stored script
- dispatch spend path / tapscript reconstruction by script or contract type
- support gap-limit discovery once the basic model is in place

Avoid initially taking on the full TS-style repository sync/watcher design. That can be layered on later.

## Why not a large enum?

A built-in enum is ergonomic for SDK-owned contract types, but it makes user-provided custom contract types second-class. The `ContractSpec + StoredContract + DynContractHandler` shape gives us:

- strong typing for built-ins
- strong typing for custom contracts
- one persistent store
- one manager API
- dynamic extension through registered handlers

## Intended integration points

In the Rust SDK this should eventually replace ad-hoc reconstruction in places like:

- default/delegate address derivation
- key discovery
- VTXO script maps for listing/spending
- repeated VHTLC reconstruction in Boltz swap methods
- watcher subscription script selection

The important invariant is: if a VTXO has a script we care about, that script should resolve to a stored contract, and that contract's registered handler should provide the spend metadata.
