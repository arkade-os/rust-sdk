# ark-grpc

gRPC transport client for Arkade servers.

This crate contains the generated Arkade gRPC types plus a Rust client wrapper used by the higher-level `ark-client` crate.

## Install

```toml
[dependencies]
ark-grpc = "0.9"
```

TLS root options are available through the `tls-native-roots` and `tls-webpki-roots` features.

## Documentation

API documentation is available on [docs.rs/ark-grpc](https://docs.rs/ark-grpc).
