# ark-rs

ark-rs is a collection of Rust crates designed to simplify building Bitcoin wallets with seamless support for both on-chain and off-chain transactions via the Ark protocol.

## Crates

- `ark-core`: Core types and utilities for Ark
- `ark-client`: Main client library for interacting with Ark servers
- `ark-grpc`: gRPC client for Ark server communication
- `ark-rest`: REST client for Ark server communication
- `ark-bdk-wallet`: Bitcoin Development Kit (BDK) integration for Ark wallets
- `e2e-tests`: End-to-end test suite

## Install

Add ark-rs to your Cargo.toml:

```toml
[dependencies]
ark-client = "0.1" # Replace with actual version
ark-core = "0.1" # Replace with actual version
```

## Usage

### Examples

- [`e2e_two_party`](./e2e-tests/tests/e2e_two_party.rs)
- [`e2e_concurrent_settlement`](./e2e-tests/tests/e2e_concurrent_settlement.rs)
- [`e2e_send_onchain_boarding_output`](./e2e-tests/tests/e2e_send_onchain_boarding_output.rs)
- [`e2e_send_onchain_vtxo_and_boarding_output`](./e2e-tests/tests/e2e_send_onchain_vtxo_and_boarding_output.rs)
- [`e2e_sub_dust.rs`](./e2e-tests/tests/e2e_sub_dust.rs)
- [`sample client`](./ark-client-sample/src/main.rs)

### Client Initialization

```rust
use ark_client::Client;
use ark_client::OfflineClient;
use bitcoin::key::Keypair;
use bitcoin::secp256k1::SecretKey;
use std::sync::Arc;

// Initialize the client
async fn init_client() -> Result<Client<MyBlockchain, MyWallet>, ark_client::Error> {
    // Create a keypair for signing transactions
    let secp = bitcoin::key::Secp256k1::new();
    let secret_key = SecretKey::from_str("your_private_key_here")?;
    let keypair = Keypair::from_secret_key(&secp, &secret_key);

    // Initialize blockchain and wallet implementations
    let blockchain = Arc::new(MyBlockchain::new("https://esplora.example.com"));
    let wallet = Arc::new(MyWallet::new());

    // Create the offline client
    let offline_client = OfflineClient::new(
        "my-ark-client".to_string(),
        keypair,
        blockchain,
        wallet,
        "https://ark-server.example.com".to_string(),
        InMemorySwapStorage::default(),
        "http://boltz.example.com".to_string(),
        timeout,
    );

    // Connect to the Ark server and get server info
    let client = offline_client.connect().await?;

    Ok(client)
}
```

### Getting Addresses

```rust
// Get an Ark address for receiving VTXOs
let (ark_address, vtxo) = client.get_offchain_address();
println!("Send VTXOs to this offchain address: {}", ark_address);

// Get a boarding address for on-chain deposits
let boarding_address = client.get_boarding_address()?;
println!("Send coins to this on-chain address: {}", boarding_address);
```

### Checking Balances

```rust
// Get off-chain balance
let balance = client.offchain_balance().await?;
println!(
    "Off-chain balance: confirmed = {}, pending = {}, total = {}",
    balance.confirmed(),
    balance.pending(),
    balance.total()
);

// List spendable VTXOs
let spendable_vtxos = client.spendable_vtxos().await?;
```

### Sending VTXOs

```rust
use ark_core::ArkAddress;
use bitcoin::Amount;

// Send VTXOs to an Ark address
async fn send_vtxo(
    client: &Client<MyBlockchain, MyWallet>,
    address: &str,
    amount_sats: u64,
) -> Result<(), ark_client::Error> {
    let ark_address = ArkAddress::decode(address)?;
    let amount = Amount::from_sat(amount_sats);

    let psbt = client.send_vtxo(ark_address, amount).await?;
    let txid = psbt.extract_tx()?.compute_txid();

    println!(
        "Sent {} sats to {} in transaction {}",
        amount_sats, address, txid
    );

    Ok(())
}
```

### Settling Transactions (Batching)

```rust
use ark_core::round::create_and_sign_forfeit_txs;
use ark_core::round::generate_nonce_tree;
use ark_core::round::sign_vtxo_tree;
use ark_core::server::RoundInput;
use ark_core::server::RoundOutput;
use ark_core::server::RoundStreamEvent;
use futures::StreamExt;

// Participate in a round to settle VTXOs and boarding outputs
async fn participate_in_round(
    client: &Client<MyBlockchain, MyWallet>,
    to_address: ArkAddress,
) -> Result<Option<bitcoin::Txid>, ark_client::Error> {
    // Get spendable VTXOs and boarding outputs
    let spendable_vtxos = client.spendable_vtxos().await?;
    let boarding_outputs = client.get_boarding_outputs().await?;

    if spendable_vtxos.is_empty() && boarding_outputs.is_empty() {
        return Ok(None);
    }

    // Create ephemeral keypair for this round
    let secp = bitcoin::key::Secp256k1::new();
    let mut rng = rand::thread_rng();
    let ephemeral_kp = bitcoin::key::Keypair::new(&secp, &mut rng);

    // Prepare round inputs
    let round_inputs = prepare_round_inputs(spendable_vtxos, boarding_outputs);

    // Register inputs for the next round
    let payment_id = client
        .network_client()
        .register_inputs_for_next_round(ephemeral_kp.public_key(), &round_inputs)
        .await?;

    // Calculate total spendable amount
    let spendable_amount = calculate_total_amount(spendable_vtxos, boarding_outputs);

    // Register outputs for the next round
    let round_outputs = vec![RoundOutput::new_virtual(to_address, spendable_amount)];
    client
        .network_client()
        .register_outputs_for_next_round(payment_id.clone(), &round_outputs)
        .await?;

    // Ping to confirm participation
    client.network_client().ping(payment_id).await?;

    // Get event stream and handle round signing
    let mut event_stream = client.network_client().get_event_stream().await?;

    // Process round events and sign as needed
    // ... (implementation details)

    Ok(Some(txid))
}
```

### Transaction History

```rust
// Get transaction history
let transactions = client.transaction_history().await?;

for tx in transactions {
    println!("Transaction: {}", tx.txid());
    println!("  Type: {}", tx.tx_type());
    println!("  Amount: {}", tx.amount());
    println!("  Timestamp: {}", tx.timestamp());
}
```

## Minimum Supported Rust Version (MSRV)

This library should compile with any combination of features on **Rust 1.75.0**.

Use `Cargo-minimal.lock` to build the MSRV by copying to `Cargo.lock` and building.

## Local Development Setup

### Prerequisites

1. Install Rust and Cargo (see https://rustup.rs/).

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

We use [just](https://github.com/casey/just) to manage common development tasks. You can install it using cargo

```bash
cargo install just
```

### Development environment

For running e2e tests locally you will need to run bitcoind, esplora, and an ark server.

To quickly fire up bitcoind we make use of [Nigiri](https://nigiri.vulpem.com/).

```bash
curl https://getnigiri.vulpem.com | bash
```

We build and run ark server from source, we have a few convenience methods:

```bash
# replace <tag> with a git tag or version, e.g. 0.4.2
just arkd-checkout <tag>
```

Build and run arkd (note, you will need to have golang installed on your machine). Please refer to [arkd's readme](https://github.com/arkade-os/arkd/) for system requirements.

```bash
# Note: the default round interval of ark server might be a bit too fast, we provide a simple patch function to change the round interval to 30 seconds
# just arkd-patch-makefile
# afterwards you can run
just arkd-setup
```

### Building the Project

1. Build the project:

```bash
cargo build
```

### Common Development Tasks

Use the following Just commands for common tasks:

```bash
# Run tests
just test

# Run e2e tests (bitcoind, arkd etc is required)
just e2e-tests

# Format code
just fmt

# Run clippy
just clippy
```

To generate code from proto files we use [tokio-prost](https://github.com/tokio-rs/prost).
It does not bundle protoc anymore, hence, you'll need to install it yourself (see [here](http://google.github.io/proto-lens/installing-protoc.html)).
Once installed you can generate the files:

```bash
RUSTFLAGS="--cfg genproto" cargo build
```

## Contributing

We welcome contributions! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
