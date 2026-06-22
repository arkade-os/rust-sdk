# ark-client

High-level client library for interacting with Arkade servers.

`ark-client` provides the main wallet/client abstractions for receiving, selecting, and sending VTXOs, boarding funds on-chain, estimating fees, watching VTXO state, and coordinating Arkade rounds through the supported transport clients.

## Install

```toml
[dependencies]
ark-client = "0.9.3"
```

Enable optional SQLite storage support with:

```toml
ark-client = { version = "0.9.3", features = ["sqlite"] }
```

## Documentation

API documentation is available on [docs.rs/ark-client](https://docs.rs/ark-client).
