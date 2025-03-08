set dotenv-load

arkd_url := "http://localhost:7070"
arkd_logs := "$PWD/arkd.log"

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

# Checkout ark (https://github.com/ark-network/ark) in a local directory
# Run with `just arkd-checkout "master"`  to checkout and sync latest master or
# `just arkd-checkout "da64028e06056b115d91588fb1103021b04008ad"`to checkout a specific commit
[positional-arguments]
arkd-checkout tag:
     #!/usr/bin/env bash

     set -euxo pipefail

     mkdir -p $ARK_GO_DIR
     cd $ARK_GO_DIR

     CHANGES_STASHED=false

     if [ -d "ark" ]; then
         # Repository exists, update it
         echo "Directory exists, refreshing..."
         cd ark

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
         git clone https://github.com/ark-network/ark.git
         cd ark
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

    just arkd-run

    echo "Started arkd"

    just arkd-init

    just arkd-fund 10

arkd-patch-makefile:
    #!/usr/bin/env bash
    set -euxo pipefail

    cd ark-go/ark/server
    # This version will match ARK_ROUND_INTERVAL=ANY_NUMBER
    # On macOS, sed requires an empty string after -i for in-place editing
    if [[ "$OSTYPE" == "darwin"* ]]; then
        # macOS
        sed -i '' 's/ARK_ROUND_INTERVAL=[0-9][0-9]*/ARK_ROUND_INTERVAL=30/' Makefile
    else
        # Linux
        sed -i 's/ARK_ROUND_INTERVAL=[0-9][0-9]*/ARK_ROUND_INTERVAL=30/' Makefile
    fi

# Start `arkd` binary.
arkd-run:
    #!/usr/bin/env bash

    set -euxo pipefail

    make -C $ARKD_DIR run &> {{arkd_logs}} &

    just _wait-until-arkd-is-ready

    echo "Arkd started. Find the logs in {{arkd_logs}}"


# Build `arkd` binary.
arkd-build:
    #!/usr/bin/env bash

    set -euxo pipefail

    make -C $ARKD_DIR build

    echo "Arkd built"



# Initialize `arkd` by creating and unlocking a new wallet.
arkd-init:
    #!/usr/bin/env bash

    set -euxo pipefail

    seed=$(curl -s {{arkd_url}}/v1/admin/wallet/seed | jq .seed -r)

    curl -s --data-binary '{"seed": "$seed", "password": "password"}' -H "Content-Type: application/json" {{arkd_url}}/v1/admin/wallet/create

    echo "Created arkd wallet"

    curl -s --data-binary '{"password" : "password"}' -H "Content-Type: application/json" {{arkd_url}}/v1/admin/wallet/unlock

    echo "Unlocked arkd wallet"

    just _wait-until-arkd-wallet-is-ready

# Fund `arkd`'s wallet with `n` utxos.
arkd-fund n:
    #!/usr/bin/env bash

    set -euxo pipefail

    for i in {1..{{n}}}; do
        address=$(curl -s {{arkd_url}}/v1/admin/wallet/address | jq .address -r)

        echo "Funding arkd wallet (Iteration $i)"

        nigiri faucet "$address" 10
    done

# Stop `arkd` binary and delete logs.
arkd-kill:
    pkill -9 arkd && echo "Stopped arkd" || echo "Arkd not running, skipped"
    [ ! -e "{{arkd_logs}}" ] || mv -f {{arkd_logs}} {{arkd_logs}}.old

# Wipe `arkd` data directory.
arkd-wipe:
    @echo Clearing arkd in $ARKD_DIR/data
    rm -rf $ARKD_DIR/data

_wait-until-arkd-is-ready:
    #!/usr/bin/env bash

    echo "Waiting for arkd to be ready..."

    for ((i=0; i<30; i+=1)); do
      status_code=$(curl -o /dev/null -s -w "%{http_code}" {{arkd_url}}/v1/admin/wallet/seed)

      if [ "$status_code" -eq 200 ]; then
        echo "arkd is ready!"
        exit 0
      fi
      sleep 1
    done

    echo "arkd was not ready in time"

    exit 1

_wait-until-arkd-wallet-is-ready:
    #!/usr/bin/env bash

    echo "Waiting for arkd wallet to be ready..."

    for ((i=0; i<30; i+=1)); do
      res=$(curl -s {{arkd_url}}/v1/admin/wallet/status)

      if echo "$res" | jq -e '.initialized == true and .unlocked == true and .synced == true' > /dev/null; then
        echo "arkd wallet is ready!"
        exit 0
      fi
      sleep 1
    done

    echo "arkd wallet was not ready in time"

    exit 1

nigiri-start:
    #!/usr/bin/env bash
    nigiri start

nigiri-wipe:
    #!/usr/bin/env bash
    nigiri stop --delete


## -------------------------
## Running tests
## -------------------------

test:
    @echo running all tests
    cargo test -- --nocapture

e2e-tests:
    @echo running e2e tests
    cargo test -p e2e-tests -- --ignored --nocapture
