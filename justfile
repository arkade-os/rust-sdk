set dotenv-load := true

arkd_admin_url := "http://localhost:7071"
arkd_wallet_port := "6060"
arkd_wallet_url := "http://localhost:" + arkd_wallet_port
arkd_logs := "$PWD/arkd.log"
arkd_wallet_logs := "$PWD/arkd-wallet.log"
introspector_port := "7073"
introspector_url := "http://127.0.0.1:" + introspector_port
introspector_logs := "$PWD/introspector.log"
introspector_image := "ark-rs-introspector:local"
introspector_container := "introspector"

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
# Checkout ark (https://github.com/arkade-os/arkd) in a local directory
# Run with `just arkd-checkout "master"`  to checkout and sync latest master or

# `just arkd-checkout "da64028e06056b115d91588fb1103021b04008ad"`to checkout a specific commit
[positional-arguments]
arkd-checkout tag:
    #!/usr/bin/env bash

    set -euxo pipefail

    mkdir -p $ARK_GO_DIR
    cd $ARK_GO_DIR

    CHANGES_STASHED=false

    if [ -d "arkd" ]; then
        # Repository exists, update it
        echo "Directory exists, refreshing..."
        cd arkd

        # Check for local changes and stash them if they exist
        if ! git diff --quiet || ! git diff --staged --quiet; then
            echo "Stashing local changes..."
            git stash push -m "Automated stash before update"
            CHANGES_STASHED=true
        fi

        git fetch --all

        # Only update master if we're not going to check it out explicitly
        if [ -z "$1" ] || [ "$1" != "master" ]; then
            # Store current branch
            CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)

            # Update master branch
            git checkout master
            git pull origin master

            # Return to original branch
            if [ "$CURRENT_BRANCH" != "master" ]; then
                git checkout "$CURRENT_BRANCH"
            fi
        fi

    else
        echo "Directory does not exist, checking it out..."
        # Clone new repository
        git clone https://github.com/arkade-os/arkd.git
        cd arkd
    fi

    if [ ! -z "$1" ]; then
        echo "Checking out " $1
        git checkout $1
    else
        echo "Checking out master"
        git checkout master
    fi

    # Reapply stashed changes if they exist
    if [ "$CHANGES_STASHED" = true ]; then
        echo "Reapplying local changes..."
        git stash pop
    fi

# Set up `arkd` so that we can run the client e2e tests against it.
arkd-setup:
    #!/usr/bin/env bash

    set -euxo pipefail

    echo "Running arkd from $ARKD_DIR"

    just arkd-wallet-run

    echo "Started arkd-wallet"

    echo "Running arkd from $ARKD_DIR"

    just arkd-run

    echo "Started arkd"

    just arkd-fund 10

arkd-patch-makefile:
    #!/usr/bin/env bash
    set -euxo pipefail

    cd $ARKD_DIR/server
    # This version will match ARK_ROUND_INTERVAL=ANY_NUMBER
    # On macOS, sed requires an empty string after -i for in-place editing
    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS
        sed -i '' 's/ARK_ROUND_INTERVAL=[0-9][0-9]*/ARK_ROUND_INTERVAL=30/' Makefile
    else
        # Linux
        sed -i 's/ARK_ROUND_INTERVAL=[0-9][0-9]*/ARK_ROUND_INTERVAL=30/' Makefile
    fi

# Start `arkd-wallet` binary.
arkd-wallet-run:
    #!/usr/bin/env bash

    set -euxo pipefail

    # Start up pg and nbxplorer
    docker compose -f $ARKD_DIR/docker-compose.regtest.yml up -d pg nbxplorer

    just _wait-for-docker-log nbxplorer "Now listening on: http://0.0.0.0:32838" 30

    make run-wallet -C $ARKD_DIR run &> {{ arkd_wallet_logs }} &

    just _wait-for-log-file {{ arkd_wallet_logs }} "arkd wallet listens on: 6060" 30

    echo "arkd wallet started. Find the logs in {{ arkd_wallet_logs }}"

# Start `arkd` binary.
arkd-run:
    #!/usr/bin/env bash

    set -euxo pipefail

    echo "Creating arkd wallet with logs in {{ arkd_logs }}"

    make -C $ARKD_DIR run-light &> {{ arkd_logs }} &

    just _wait-for-log-file {{ arkd_logs }} "started listening at :7070" 300
    just _wait-for-log-file {{ arkd_logs }} "started admin listening at :7071" 30

    just _create-arkd

    echo "Created arkd wallet"

    just arkd-init

    echo "arkd started. Find the logs in {{ arkd_logs }}"

# Initialize `arkd` by creating and unlocking a new wallet.
arkd-init:
    #!/usr/bin/env bash

    set -euxo pipefail

    curl -fsS --data-binary '{"password" : "password"}' -H "Content-Type: application/json" {{ arkd_admin_url }}/v1/admin/wallet/unlock

    echo "Unlocked arkd wallet"

    just _wait-until-arkd-is-initialized

# Build `arkd` binary and others.
arkd-build:
    #!/usr/bin/env bash

    set -euxo pipefail

    make -C $ARKD_DIR build-all

    echo "arkd built"

# Fund `arkd`'s wallet with `n` utxos.
arkd-fund n:
    #!/usr/bin/env bash

    set -euxo pipefail

    for i in {1..{{ n }}}; do
        address=$(curl -fsS -X 'POST' \
                    {{ arkd_wallet_url }}/v1/wallet/derive-addresses \
                    -H 'accept: application/json' \
                    -H 'Content-Type: application/json' \
                    -d '{
                    "num": 1
                  }' | jq -r '.addresses[0]'
        )

        echo "Funding arkd wallet (Iteration $i)"

        nigiri faucet "$address" 10
    done

# Stop `arkd` binary and delete logs.
arkd-kill:
    pkill -9 arkd && echo "Stopped arkd" || echo "arkd not running, skipped"
    [ ! -e "{{ arkd_logs }}" ] || mv -f {{ arkd_logs }} {{ arkd_logs }}.old

# Stop `arkd-wallet` binary and delete logs.
arkd-wallet-kill:
    #!/usr/bin/env bash
    pid=$(lsof -ti :{{ arkd_wallet_port }})
    if [ -n "$pid" ]; then \
        kill -9 $pid && echo "Stopped arkd wallet on port {{ arkd_wallet_port }}" || echo "Failed to stop arkd wallet"; \
    else \
        echo "No process found on port {{ arkd_wallet_port }}"; \
    fi
    [ ! -e "{{ arkd_wallet_logs }}" ] || mv -f {{ arkd_wallet_logs }} {{ arkd_wallet_logs }}.old

# Checkout introspector (https://github.com/ArkLabsHQ/introspector) in a local directory.
[positional-arguments]
introspector-checkout tag:
    #!/usr/bin/env bash

    set -euxo pipefail

    mkdir -p $INTROSPECTOR_GO_DIR
    cd $INTROSPECTOR_GO_DIR

    CHANGES_STASHED=false

    if [ -d "introspector" ]; then
        cd introspector

        if ! git diff --quiet || ! git diff --staged --quiet; then
            git stash push -m "Automated stash before update"
            CHANGES_STASHED=true
        fi

        git fetch --all
    else
        git clone https://github.com/ArkLabsHQ/introspector.git
        cd introspector
    fi

    if [ ! -z "$1" ]; then
        git checkout $1
    else
        git checkout master
    fi

    if [ "$CHANGES_STASHED" = true ]; then
        git stash pop
    fi

# Build the introspector docker image from source.
introspector-docker-build:
    #!/usr/bin/env bash

    set -euxo pipefail

    docker build -t {{ introspector_image }} "$INTROSPECTOR_DIR"

# Pull the introspector docker image.
introspector-docker-pull:
    #!/usr/bin/env bash

    set -euxo pipefail

    image="${INTROSPECTOR_IMAGE:-ghcr.io/arklabshq/introspector:latest}"
    docker pull "$image"

# Run introspector in docker against host arkd.
introspector-docker-run:
    #!/usr/bin/env bash

    set -euxo pipefail

    image="${INTROSPECTOR_IMAGE:-{{ introspector_image }}}"

    if ! docker image inspect "$image" > /dev/null 2>&1; then
        echo "Docker image $image not found locally"
        echo "Build it with 'just introspector-docker-build' or set INTROSPECTOR_IMAGE to a pulled image"
        exit 1
    fi

    docker rm -f {{ introspector_container }} || true

    docker run -d \
        --name {{ introspector_container }} \
        --network nigiri \
        --add-host=host.docker.internal:host-gateway \
        -p {{ introspector_port }}:7073 \
        -e INTROSPECTOR_SECRET_KEY=5646b2e23bbb82491fb4ef262079ff17594d5e873fc6bcea5f1453edbe1029b1 \
        -e INTROSPECTOR_NO_TLS=true \
        -e INTROSPECTOR_ARKD_URL=host.docker.internal:7070 \
        -e INTROSPECTOR_LOG_LEVEL=6 \
        "$image"

    just _wait-for-http {{ introspector_url }}/v1/info 60
    docker logs {{ introspector_container }} &> {{ introspector_logs }} || true

# Build and run introspector in docker from source.
[positional-arguments]
introspector-docker-setup tag='master':
    just introspector-checkout {{ tag }}
    just introspector-docker-build
    just introspector-docker-run

# Stop introspector docker container and save logs.
introspector-docker-kill:
    #!/usr/bin/env bash

    docker logs {{ introspector_container }} &> {{ introspector_logs }} || true
    docker rm -f {{ introspector_container }} && echo "Stopped introspector" || echo "introspector not running, skipped"

# Wipe docker containers set up from the `arkd` repo.
docker-wipe:
    @echo Stopping arkd-related docker containers
    make docker-stop -C $ARKD_DIR

# Wipe `arkd` data directory.
arkd-wipe:
    @echo Clearing $ARKD_DIR/data
    rm -rf $ARKD_DIR/data

# Wipe `arkd-wallet` data directory.
arkd-wallet-wipe:
    @echo Clearing $ARKD_DIR/data
    rm -rf $ARKD_DIR/data

_create-arkd:
    #!/usr/bin/env bash

    echo "Waiting for arkd wallet seed to be ready..."

    for ((i=0; i<30; i+=1)); do
      seed=$(curl -fsS {{ arkd_admin_url }}/v1/admin/wallet/seed | jq .seed -r)

      if [ -n "$seed" ]; then
        echo "arkd wallet seed is ready! Creating wallet"
        curl -fsS --data-binary "{\"seed\": \"$seed\", \"password\": \"password\"}" -H "Content-Type: application/json" {{ arkd_admin_url }}/v1/admin/wallet/create
        exit 0
      fi
      sleep 1
    done

    echo "arkd wallet seed was not ready in time"

    exit 1

_wait-until-arkd-is-initialized:
    #!/usr/bin/env bash

    echo "Waiting for arkd wallet to be initialized..."

    for ((i=0; i<30; i+=1)); do
      res=$(curl -fsS {{ arkd_admin_url }}/v1/admin/wallet/status)

      if echo "$res" | jq -e '.initialized == true and .unlocked == true and .synced == true' > /dev/null; then
        echo "arkd wallet is initialized!"
        exit 0
      fi
      sleep 1
    done

    echo "arkd wallet was not initialized in time"

    exit 1

# Wait for an HTTP endpoint to become available.

# Usage: just _wait-for-http url timeout_seconds
[positional-arguments]
_wait-for-http url timeout:
    #!/usr/bin/env bash

    set -euo pipefail

    URL="${1}"
    TIMEOUT="${2}"

    echo "Waiting for HTTP endpoint '${URL}' (timeout: ${TIMEOUT}s)..."

    for ((i=0; i<${TIMEOUT}; i+=1)); do
      if curl -fsS "${URL}" > /dev/null; then
        echo "HTTP endpoint '${URL}' is ready!"
        exit 0
      fi
      sleep 1
    done

    echo "HTTP endpoint '${URL}' was not ready within ${TIMEOUT} seconds"
    exit 1

# Wait for a specific log pattern in a docker container

# Usage: just _wait-for-docker-log container_name "log_pattern" timeout_seconds
[positional-arguments]
_wait-for-docker-log container pattern timeout:
    #!/usr/bin/env bash

    set -euo pipefail

    CONTAINER="${1}"
    PATTERN="${2}"
    TIMEOUT="${3}"

    echo "Waiting for log pattern '${PATTERN}' in container '${CONTAINER}' (timeout: ${TIMEOUT}s)..."

    for ((i=0; i<${TIMEOUT}; i+=1)); do
      if docker logs "${CONTAINER}" 2>&1 | grep -q "${PATTERN}"; then
        echo "Found log pattern '${PATTERN}' in container '${CONTAINER}'!"
        exit 0
      fi
      sleep 1
    done

    echo "Log pattern '${PATTERN}' not found in container '${CONTAINER}' within ${TIMEOUT} seconds"
    exit 1

# Wait for a specific log pattern in a log file

# Usage: just _wait-for-log-file log_file_path "log_pattern" timeout_seconds
[positional-arguments]
_wait-for-log-file file pattern timeout:
    #!/usr/bin/env bash

    set -euo pipefail

    FILE="${1}"
    PATTERN="${2}"
    TIMEOUT="${3}"

    echo "Waiting for log pattern '${PATTERN}' in file '${FILE}' (timeout: ${TIMEOUT}s)..."

    for ((i=0; i<${TIMEOUT}; i+=1)); do
      if [ -f "${FILE}" ] && grep -q "${PATTERN}" "${FILE}"; then
        echo "Found log pattern '${PATTERN}' in file '${FILE}'!"
        exit 0
      fi
      sleep 1
    done

    echo "Log pattern '${PATTERN}' not found in file '${FILE}' within ${TIMEOUT} seconds"
    exit 1

nigiri-start:
    #!/usr/bin/env bash
    nigiri start

nigiri-wipe:
    #!/usr/bin/env bash
    nigiri stop --delete

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

# Run all e2e tests (arkd must be running locally).
e2e-tests:
    @echo running e2e tests
    cargo test -p e2e-tests -- --ignored --nocapture

# Restart e2e test environment (arkd master) and run all e2e tests.
e2e-full:
    @echo running integration tests
    nigiri stop --delete && just arkd-kill arkd-wipe arkd-wallet-kill arkd-wallet-wipe introspector-docker-kill docker-wipe
    nigiri start
    sleep 1
    just arkd-build
    just arkd-setup
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
