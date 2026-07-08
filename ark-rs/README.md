# ark-rs

Convenience crate for the Arkade Rust SDK.

`ark-rs` re-exports the core SDK crates behind feature flags so applications can depend on a single package when building Arkade-enabled Bitcoin wallets.

## Install

```toml
[dependencies]
ark-rs = "0.10.1"
```

By default this includes `ark-core` and enables native TLS roots for optional transport/client crates.

Common feature flags:

- `client`: re-export `ark-client`
- `grpc`: re-export `ark-grpc`
- `sqlite`: forward SQLite support to `ark-client`
- `tls-native-roots`: use native TLS roots
- `tls-webpki-roots`: use webpki TLS roots

## Documentation

API documentation is available on [docs.rs/ark-rs](https://docs.rs/ark-rs).
