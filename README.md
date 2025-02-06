# Ark-rs - A rust client library for Ark

## Local Development Setup

### Prerequisites

1. Install Rust and Cargo (see https://rustup.rs/)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

2. We use [just](https://github.com/casey/just) to manage common development tasks. You can install it using cargo

```bash
cargo install just
```

### Development environment

For running e2e tests locally you will need to run bitcoind, esplora, and an ark server.

1. To quickly fire up bitcoind we make use of [Nigiri](https://nigiri.vulpem.com/).

```bash
curl https://getnigiri.vulpem.com | bash
```

2. We build and run ark server from source, we have a few convenience methods:

```bash
# replace <tag> with a git tag or version, e.g. 0.4.2
just clarkd-checkout <tag>
```

3. Build and run clarkd (note, you will need to have golang installed on your machine). Please refer to [ark's readme](https://github.com/ark-network/ark/) for system requirements.

```bash
# Note: the default round interval of ark server might be a bit too fast, we provide a simple patch function to change the round interval to 30 seconds
# just clarkd-patch-makefile     
# afterwards you can run
just clarkd-setup
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

# Run e2e tests (bitcoind, clarkd etc is required)
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
RUSTFLAGS="--cfg genproto" cargo build`
```
