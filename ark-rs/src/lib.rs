//! Convenience crate that re-exports the Arkade Rust SDK behind feature flags.
//!
//! Arkade is an open execution engine for Bitcoin. Every Arkade transaction is a Bitcoin
//! transaction, and every virtual output is a Taproot output with at least two spending paths: an
//! instant path through the operator, and a delayed path the owner can take alone.
//!
//! This crate pulls the SDK together in one dependency. Each module is a re-export:
//!
//! | Module | Crate | Feature | Purpose |
//! | --- | --- | --- | --- |
//! | [`core`] | [`ark_core`] | always on | Core Arkade types and transaction utilities. |
//! | [`client`] | [`ark_client`] | `client` | Wallet abstractions: send, receive, board, settle. |
//! | [`grpc`] | [`ark_grpc`] | `grpc` | gRPC transport client for the operator. |
//!
//! # Install
//!
//! ```toml
//! [dependencies]
//! ark-rs = { version = "0.10.1", features = ["client", "grpc"] }
//! ```
//!
//! `ark-core` is always available. The `client` and `grpc` modules are optional, so enable the
//! features you need. To talk to the operator over REST instead of gRPC, depend on
//! [`ark-rest`](https://docs.rs/ark-rest) directly.
//!
//! Two more features forward to the crates below:
//!
//! - `sqlite` — SQLite storage in `ark-client`.
//! - `tls-native-roots` (default) or `tls-webpki-roots` — the TLS root store for both transports.
//!
//! # Where to start
//!
//! Read the [`ark_client::Client`] documentation. It covers the wallet lifecycle end to end.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "client")]
#[cfg_attr(docsrs, doc(cfg(feature = "client")))]
pub use ark_client as client;
pub use ark_core as core;
#[cfg(feature = "grpc")]
#[cfg_attr(docsrs, doc(cfg(feature = "grpc")))]
pub use ark_grpc as grpc;
