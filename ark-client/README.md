# ark-client

High-level client library for building Arkade wallets in Rust.

`ark-client` provides the wallet abstractions of the SDK. Use it to receive, select, and send virtual outputs, board funds from onchain, estimate fees, track the state of a virtual output, and join batch swaps through the operator. It talks to the operator through either supported transport client, `ark-grpc` or `ark-rest`.

## Install

```toml
[dependencies]
ark-client = "0.10.1"
```

Enable optional SQLite storage support with:

```toml
ark-client = { version = "0.10.1", features = ["sqlite"] }
```

## Documentation

API documentation is available on [docs.rs/ark-client](https://docs.rs/ark-client).
