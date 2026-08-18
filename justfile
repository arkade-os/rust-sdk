set dotenv-load := true

# The regtest stack (Bitcoin Core + Fulcrum + mempool/esplora + arkd + emulator)
# is provided by the `regtest` git submodule (arkade-regtest) and driven by its
# zero-dependency Node CLI (`regtest.mjs`). `.env.regtest` holds this repo's arkd
# image + config overrides (exit delays, zeroed intent fees).
regtest_dir := "regtest"
regtest_env := ".env.regtest"
# Profiles the e2e suite needs: `emulator` transitively pulls in `base` + `ark`
# (arkd) plus the arkade-script emulator used by the introspector tests. boltz /
# delegate / solver are not exercised by the `e2e_*` suite.
regtest_profiles := "emulator"

mod ark-rest
mod nix

## ------------------------
## Code quality functions
## ------------------------

fmt:
    dprint fmt

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# TODO: We should build `ark-core`, `ark-rest`, `ark-bdk-wallet` and eventually even `ark-client`
# for WASM.

build-wasm:
    cargo build -p ark-core --target wasm32-unknown-unknown
    cargo build -p ark-rest --target wasm32-unknown-unknown

## -----------------
## Code generation
## -----------------

# Generate GRPC code. Modify proto files before calling this.
gen-grpc:
    #!/usr/bin/env bash

    RUSTFLAGS="--cfg genproto" cargo build -p ark-grpc

## -------------------------
## Local development setup
## -------------------------

# Initialize / update the `regtest` (arkade-regtest) submodule.
regtest-init:
    git submodule update --init --recursive {{ regtest_dir }}

# Start the regtest stack (arkd is run from ARKD_IMAGE; the stack self-funds it).
regtest-start:
    node {{ regtest_dir }}/regtest.mjs start --env {{ regtest_env }} --profile {{ regtest_profiles }}

# Stop the regtest stack (preserves data/volumes).
regtest-stop:
    node {{ regtest_dir }}/regtest.mjs stop

# Remove the regtest stack's containers and volumes.
regtest-clean:
    node {{ regtest_dir }}/regtest.mjs clean

# Faucet `amount` BTC to `address`, mining 1 block to confirm it.
[positional-arguments]
faucet address amount:
    node {{ regtest_dir }}/regtest.mjs faucet "$1" "$2" --confirm

# Mine `n` blocks.
mine n='1':
    node {{ regtest_dir }}/regtest.mjs mine {{ n }}

## -------------------------
## Ark sample commands
## -------------------------

mod ark-sample 'ark-client-sample/justfile'

## -------------------------
## Running tests
## -------------------------

# Run all unit tests.
test:
    @echo running all tests
    cargo test -- --nocapture

# Run all e2e tests (the regtest stack must be running locally).
e2e-tests:
    @echo running e2e tests
    cargo test -p e2e-tests -- --ignored --nocapture

# Restart the regtest environment and run all e2e tests.
e2e-full:
    @echo running integration tests
    just regtest-clean
    just regtest-start
    just e2e-tests

# Test WASM functionality (requires wasm-pack and running Ark server on localhost:7070).
wasm-test:
    #!/usr/bin/env bash
    cd ark-rest

    echo "Running WASM tests..."
    echo "Note: Requires Ark server running on http://localhost:7070"

    # Check if wasm-pack is installed
    if ! command -v wasm-pack &> /dev/null; then
        echo "wasm-pack not found."
        exit 1
    fi

    # Run WASM tests with Firefox (headless)
    wasm-pack test --headless --firefox -- --test wasm

# Check MSRV for all published crates.
msrv-check:
    #!/usr/bin/env bash

    packages=(
        "ark-core"
        "ark-grpc"
        "ark-rest"
        "ark-client"
        "ark-bdk-wallet"
        "ark-rs"
    )

    root_dir="$PWD"

    for pkg in "${packages[@]}"; do
        echo "=== Checking MSRV for $pkg ==="
        cd "$root_dir/$pkg"
        cargo msrv verify 2>&1 || true
        echo ""
    done

    echo "=== MSRV check complete ==="
