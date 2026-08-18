# ark-rs

Rust crates for building Bitcoin wallets and applications with Arkade.

[![crates.io](https://img.shields.io/crates/v/ark-rs)](https://crates.io/crates/ark-rs)
[![docs.rs](https://img.shields.io/docsrs/ark-rs)](https://docs.rs/ark-rs)

This repository contains the Arkade Rust SDK: core types, transport clients, wallet integration, fee estimation, and development and test utilities.

## Crates

| Crate                                                  | Purpose                                                                                                 |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| [`ark-rs`](./ark-rs)                                   | Convenience crate that re-exports the main SDK crates behind feature flags.                             |
| [`ark-core`](./ark-core)                               | Core Arkade protocol types and transaction utilities.                                                   |
| [`ark-client`](./ark-client)                           | High-level client library for building Arkade wallets in Rust.                                          |
| [`ark-grpc`](./ark-grpc)                               | gRPC transport client for the Arkade operator.                                                          |
| [`ark-rest`](./ark-rest)                               | REST transport client for the Arkade operator.                                                          |
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
ark-rs = "0.10.1"
```

Or depend on the crates you need directly:

```toml
[dependencies]
ark-core = "0.10.1"
ark-client = "0.10.1"
ark-bdk-wallet = "0.10.1"
```

Optional `ark-rs` features:

- `client`: re-export `ark-client`
- `grpc`: re-export `ark-grpc`
- `sqlite`: enable SQLite storage support in `ark-client`
- `tls-native-roots`: use native TLS roots
- `tls-webpki-roots`: use webpki TLS roots

## Examples and documentation

- API documentation is published on [docs.rs/ark-rs](https://docs.rs/ark-rs).
- The [`ark-client-sample`](./ark-client-sample) crate shows how to wire the client in a CLI application.
- The [`e2e-tests`](./e2e-tests/tests) directory contains integration examples that run against a local operator.

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

Run end-to-end tests against a local regtest environment. The stack (Bitcoin
Core + Fulcrum + mempool/esplora + arkd + emulator) is provided by the `regtest`
git submodule ([arkade-regtest](https://github.com/ArkLabsHQ/arkade-regtest))
and driven by its Node CLI; it requires Docker and Node.js:

```bash
just regtest-init    # initialize the regtest submodule (first time only)
just regtest-start   # bring up the stack (arkd runs from a container image)
just e2e-tests       # run the e2e suite
just regtest-clean   # tear the stack down
```

See `just --list` for the full set of local development, regtest, WASM, and release helper commands.

## Minimum supported Rust version

The SDK supports Rust **1.86.0**.

Use the checked-in `Cargo-minimal.lock` when validating the MSRV:

```bash
just msrv-check
```

## License

This project is licensed under the MIT License. See [`LICENSE`](./LICENSE).
