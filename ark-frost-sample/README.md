# ark-frost-sample

An Ark Lightning client whose wallet key is a **FROST 2-of-3 threshold multisig** ([RFC 9591](https://www.rfc-editor.org/rfc/rfc9591), via [`frost-secp256k1-tr`](https://crates.io/crates/frost-secp256k1-tr)).

Three actors — **alice**, **bob** and **clair** — each hold a share of a single group key created with distributed key generation: no party ever knows the full secret key, at any point. The Arkade address is derived from the x-only group public key, so to the Ark server (and on chain) the wallet is indistinguishable from a single-key wallet.

Every signature needs 2 of the 3 actors. Coordination happens by **copy-pasting base64 blobs between terminals**: the coordinator prints a signing request, one other actor answers it with `sign`, and the coordinator aggregates the shares into a plain BIP340 signature.

The sample plugs into `ark-client` through the `Signer` trait, which routes all Schnorr script-path signing through the FROST ceremony instead of a local keypair.

## Supported flows

- **Receive via Lightning** (`invoice`): Boltz reverse submarine swap; claiming the VHTLC requires one signing ceremony.
- **Send via Lightning** (`pay`): Boltz submarine swap; funding the VHTLC requires signing ceremonies (Ark tx + checkpoint tx).
- `balance`, `offchain-address`, `group-info`.

The library supports all flows through the `Signer` trait, including settlement, boarding and chain swaps. Note that settlement runs against server-timed batch rounds, so the copy-paste signing ceremony of this sample may be too slow for it in practice; the Lightning flows above are the ones exercised by this walkthrough.

## Walkthrough (regtest)

Start the regtest stack (Ark server on `:7070`, esplora on `:3000`, Boltz on `:9001`), then open **three terminals** in this directory.

### 1. Key generation (all three terminals at once)

```sh
# terminal 1            # terminal 2            # terminal 3
cargo run -p ark-frost-sample -- --actor alice keygen
cargo run -p ark-frost-sample -- --actor bob   keygen
cargo run -p ark-frost-sample -- --actor clair keygen
```

Each terminal prints a **round 1 blob** — paste it into _both_ other terminals. Each terminal then prints two **round 2 blobs**, one addressed to each peer — paste each into the terminal it is addressed to. When done, each actor has its key share in `<actor>/frost_key_package.hex` and all three print the same group public key.

### 2. Receive over Lightning

In alice's terminal (any actor can coordinate):

```sh
cargo run -p ark-frost-sample -- --actor alice invoice 50000
```

Pay the printed BOLT11 invoice from any Lightning wallet. Once Boltz funds the VHTLC, alice prints a **signing request blob**. Hand it to bob (or clair):

```sh
cargo run -p ark-frost-sample -- --actor bob sign <blob>
```

Paste bob's response back into alice's terminal — the claim is signed 2-of-3 and the funds land at the multisig Ark address.

### 3. Send over Lightning

```sh
cargo run -p ark-frost-sample -- --actor alice pay <bolt11-invoice>
```

Funding the swap triggers signing ceremonies (one per transaction being signed: the Ark tx and its checkpoint tx). For each printed request, run `sign` as any other actor and paste the response back.

### Testing the ceremony without a server

`sign-test` runs a full signing ceremony over an arbitrary 32-byte message — useful to verify the keygen output before touching real funds:

```sh
cargo run -p ark-frost-sample -- --actor alice sign-test $(printf '07%.0s' {1..32})
# then answer the printed request from another terminal:
cargo run -p ark-frost-sample -- --actor clair sign <blob>
```

### 4. Balance

```sh
cargo run -p ark-frost-sample -- --actor alice balance
```

## How the ceremony fits in one round trip

FROST signing normally takes two rounds (commit, then sign). With exactly two participants the responder can do both at once: the coordinator's commitments are in the request, so the responder's commitment set is already complete and it returns its commitments _and_ its signature share in a single reply. One paste each way per signature.

The signing request contains the raw 32-byte sighash. The responder prints it before signing — in a real deployment each co-signer would independently reconstruct and verify the transaction before signing, rather than trusting the coordinator.

## Files per actor

| File                       | Contents                                                 |
| -------------------------- | -------------------------------------------------------- |
| `ark.config.toml`          | Ark server / esplora / Boltz URLs                        |
| `frost_key_package.hex`    | this actor's secret key share (never leaves the machine) |
| `frost_pubkey_package.hex` | group public key package (same for everyone)             |
| `swap_storage.sqlite`      | Boltz swap state                                         |
