# ark-rs

Rust crates for building Bitcoin wallets and applications that use the Arkade protocol.

This repository contains the Arkade Rust SDK: protocol types, transport clients, wallet integration, fee estimation, and development/test utilities.

## Crates

| Crate                                                  | Purpose                                                                                                 |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| [`ark-rs`](./ark-rs)                                   | Convenience crate that re-exports the main SDK crates behind feature flags.                             |
| [`ark-core`](./ark-core)                               | Core Arkade protocol types and transaction utilities.                                                   |
| [`ark-client`](./ark-client)                           | High-level client library for interacting with Arkade servers.                                          |
| [`ark-grpc`](./ark-grpc)                               | gRPC transport client for Arkade servers.                                                               |
| [`ark-rest`](./ark-rest)                               | REST transport client for Arkade servers.                                                               |
| [`ark-bdk-wallet`](./ark-bdk-wallet)                   | [`bdk_wallet`](https://crates.io/crates/bdk_wallet)-based implementation of `ark-client` wallet traits. |
| [`ark-fees`](./ark-fees)                               | CEL-based fee estimation library for Arkade transactions.                                               |
| [`ark-delegator`](./ark-delegator)                     | REST client for Arkade delegator services.                                                              |
| [`ark-script`](./ark-script)                           | Arkade script, taproot, opcode, and key-tweaking helpers.                                               |
| [`ark-introspector-client`](./ark-introspector-client) | Client for the Arkade introspector service.                                                             |

The repository also includes [`ark-client-sample`](./ark-client-sample) and [`e2e-tests`](./e2e-tests), which are not published to crates.io.

## Installation

Use the convenience crate if you want a single SDK dependency:

```toml
[dependencies]
ark-rs = "0.9.1"
```

Or depend on the crates you need directly:

```toml
[dependencies]
ark-core = "0.9.1"
ark-client = "0.9.1"
ark-bdk-wallet = "0.9.1"
```

Optional `ark-rs` features:

- `client`: re-export `ark-client`
- `grpc`: re-export `ark-grpc`
- `sqlite`: enable SQLite storage support in `ark-client`
- `tls-native-roots`: use native TLS roots
- `tls-webpki-roots`: use webpki TLS roots

## Examples and documentation

- API documentation is published on [docs.rs](https://docs.rs/releases/search?query=ark-rs).
- The [`ark-client-sample`](./ark-client-sample) crate shows how to wire the client in a CLI application.
- The [`e2e-tests`](./e2e-tests/tests) directory contains integration examples against a local Arkade server.

## Development

Common commands are defined in the [`justfile`](./justfile):

```bash
just fmt
just clippy
just test
```

Generate gRPC code after changing proto files:

```bash
just gen-grpc
```

Run end-to-end tests against a local `arkd` environment:

```bash
just arkd-setup
just e2e-tests
```

See `just --list` for the full set of local development, Arkade server, introspector, WASM, and release helper commands.

## Minimum supported Rust version

The SDK supports Rust **1.86.0**.

Use the checked-in `Cargo-minimal.lock` when validating the MSRV:

```bash
just msrv-check
```

## License

This project is licensed under the MIT License. See [`LICENSE`](./LICENSE).
