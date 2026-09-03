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

## Renewing Virtual Outputs

Every virtual output has an expiry. It is created inside a batch output, and
that batch output is swept once its expiry passes. **Renewal moves a virtual
output into a fresh batch swap before that happens.** A wallet that never
renews will eventually hold outputs it can no longer spend through the
operator.

Renewal is a settlement: the client hands the operator a set of inputs and
receives new confirmed virtual outputs in the next batch swap.

### Choosing a Method

| Method                                                                                                        | Settles                                                            | Use it when                                    |
| ------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ---------------------------------------------- |
| [`Client::settle`](https://docs.rs/ark-client/latest/ark_client/struct.Client.html#method.settle)             | Expired and recoverable virtual outputs, plus all boarding outputs | Periodic renewal. This is the default choice.  |
| [`Client::settle_all`](https://docs.rs/ark-client/latest/ark_client/struct.Client.html#method.settle_all)     | Everything, including healthy virtual outputs                      | Rescuing isolated sub-dust amounts. See below. |
| [`Client::settle_vtxos`](https://docs.rs/ark-client/latest/ark_client/struct.Client.html#method.settle_vtxos) | Exactly the outpoints you name                                     | You are managing selection yourself.           |

Prefer `settle`. It leaves healthy virtual outputs alone, because including
them only inflates batch fees without buying anything. Boarding outputs are
always included, since freshly funded coins normally want to enter Arkade.

Each returns `Ok(None)` when there was nothing to settle, so an empty wallet or
a wallet with nothing due is not an error.

```rust,no_run
// Renew whatever is due. Safe to call on a timer.
match client.settle(&mut rng).await? {
    Some(commitment_txid) => println!("renewed in {commitment_txid}"),
    None => println!("nothing due"),
}
```

### Sub-Dust Amounts

A settlement cannot produce a virtual output below the operator's dust
threshold. If the recoverable amounts `settle` picks up do not add up to dust
on their own, the batch swap is refused with `cannot settle into sub-dust VTXO`.

Fall back to [`Client::settle_all`](https://docs.rs/ark-client/latest/ark_client/struct.Client.html#method.settle_all) in that case. It rolls the sub-dust amounts
in alongside healthy virtual outputs, which carry enough value to clear the
threshold.

### Renewing Automatically

[`Client::start_vtxo_watcher`](https://docs.rs/ark-client/latest/ark_client/struct.Client.html#method.start_vtxo_watcher) runs renewal in the background. It:

1. delegates new virtual outputs to the delegate service for future renewal;
2. self-renews anything close to expiry, as a safety net, on a 60-second tick;
3. rotates funds off deprecated operator signers, unless you turn that arm off
   with [`VtxoWatcherConfig::migrate_deprecated_signers`](https://docs.rs/ark-client/latest/ark_client/vtxo_watcher/struct.VtxoWatcherConfig.html#structfield.migrate_deprecated_signers).

The safety net acts on virtual outputs with **less than 10% of their lifetime
remaining**, and on anything already recoverable. It skips the pass when the
selected amounts do not reach dust, for the reason above.

```rust,no_run
use std::sync::Arc;
use ark_client::vtxo_watcher::VtxoWatcherConfig;

let client = Arc::new(client);
let handle = client.start_vtxo_watcher(delegator, VtxoWatcherConfig::default());
```

The watcher needs the client behind an `Arc`, and reconnects on stream errors
with exponential backoff. Renewal stops when the returned handle is dropped, so
hold onto it for as long as the wallet is running.

### If the Window Is Missed

A virtual output whose batch output was swept is **recoverable**, not lost. The
funds are still yours at that script. What you give up is the ability to exit
unilaterally, so recover it by settling rather than by exiting: both `settle`
and the watcher already treat recoverable outputs as due.

## Documentation

API documentation is available on [docs.rs/ark-client](https://docs.rs/ark-client).
