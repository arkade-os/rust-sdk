# Cargo-minimal.lock

`Cargo-minimal.lock` pins every dependency to its lowest acceptable version.
CI copies it to `Cargo.lock` and builds with the MSRV toolchain to guarantee
the declared lower bounds actually compile.

## How to update after a dependency change

Start from the existing lockfile and let Cargo resolve only the parts that
changed.

```bash
# 1. Copy the minimal lockfile into place
cp Cargo-minimal.lock Cargo.lock

# 2. Let Cargo update only what changed
cargo check --workspace --exclude e2e-tests --exclude ark-client-sample

# 3. Save the result
cp Cargo.lock Cargo-minimal.lock

# 4. Restore the normal lockfile
cargo generate-lockfile

# 5. Verify it builds with the MSRV toolchain
cp Cargo-minimal.lock Cargo.lock
rustup run <msrv> cargo check --workspace --exclude e2e-tests --exclude ark-client-sample
cargo generate-lockfile   # restore again
```

## Troubleshooting

If step 2 pulls in a transitive dependency version that doesn't compile under
the MSRV, you'll need to manually bump it. Add a version constraint to the
relevant `Cargo.toml`, or run:

```bash
cargo update -p <broken-crate> --precise <working-version>
```

Then re-save `Cargo-minimal.lock` and restore the normal lockfile.
