# ark-grpc

gRPC transport client for the Arkade operator.

This crate contains the generated Arkade gRPC types plus a Rust client wrapper used by the higher-level `ark-client` crate.

## Install

```toml
[dependencies]
ark-grpc = "0.10.1"
```

TLS root options are available through the `tls-native-roots` and `tls-webpki-roots` features.

## Updating the generated client

See [docs/update-server-api.md](../docs/update-server-api.md) for how to regenerate the client against a new arkd release.

## Documentation

API documentation is available on [docs.rs/ark-grpc](https://docs.rs/ark-grpc).
