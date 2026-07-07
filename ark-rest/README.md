# ark-rest

REST transport client for Arkade servers.

This crate contains generated REST API bindings plus a Rust client wrapper for Arkade service, indexer, admin, signer manager, and wallet endpoints.

## Install

```toml
[dependencies]
ark-rest = "0.10.0"
```

## Notes

The low-level API modules are generated from the Ark OpenAPI specification and are exposed for advanced use. Most applications should prefer the higher-level `Client` wrapper exported by this crate, or `ark-client` for full wallet/client functionality.

## Documentation

API documentation is available on [docs.rs/ark-rest](https://docs.rs/ark-rest).
