# Updating the server API bindings (ark-grpc & ark-rest)

The `ark-grpc` and `ark-rest` crates are generated from the API spec of the Ark
server, [`arkade-os/arkd`](https://github.com/arkade-os/arkd). When a new arkd
version is released, follow this guide to bump both crates.

## 1. Check out the arkd release

We keep a local clone of arkd at `ark-go/arkd` (gitignored). If you don't have
it yet:

```bash
mkdir -p ark-go
git clone git@github.com:arkade-os/arkd.git ark-go/arkd
```

Check out the latest release (preferred; use `master` only if you need
unreleased changes):

```bash
cd ark-go/arkd
git fetch --tags origin
git checkout $(git tag --sort=-v:refname | head -1)
```

The spec files live under `ark-go/arkd/api-spec/`:

- protobuf: `api-spec/protobuf/ark/v1/*.proto`
- swagger: `api-spec/openapi/swagger/ark/v1/*.openapi.json`

## 2. Bump ark-grpc

### Copy the proto files

```bash
cp ark-go/arkd/api-spec/protobuf/ark/v1/*.proto ark-grpc/proto/ark/v1/
```

The vendored protos under `ark-grpc/proto/google/` and
`ark-grpc/proto/vendor/meshapi/` are not part of arkd; they rarely change and
only need attention if proto compilation fails on a missing import.

If arkd added a _new_ proto file, also add it to the `compile_protos` list in
`ark-grpc/build.rs`.

### Regenerate the client

Code generation is gated behind a cfg flag (see `ark-grpc/build.rs`) and writes
into `ark-grpc/src/generated/`, which is committed:

```bash
cd ark-grpc
RUSTFLAGS="--cfg genproto" cargo build
```

Review the diff in `src/generated/`, then make the crate compile again: the
hand-written wrapper in `ark-grpc/src/client.rs` / `types.rs` may need updates
for renamed or changed messages.

## 3. Bump ark-rest

> ⚠️ Unlike ark-grpc, the generated files in `ark-rest/src/` are **heavily
> hand-modified**. Do not blindly accept the generator output — only keep what
> is genuinely new.

### Copy the swagger files

```bash
cp ark-go/arkd/api-spec/openapi/swagger/ark/v1/*.openapi.json ark-rest/swagger/
```

Note: arkd also ships `admin.openapi.json`, which we intentionally do not
vendor — the admin API is not part of `ark-rest`. Delete it if it got copied
over.

### Merge and generate

```bash
cd ark-rest
just merge-swagger   # produces swagger/merged.openapi.json
just generate        # runs openapi-generator over the merged spec, in place
```

### Review the diff carefully

The generator overwrites our hand-edited files. `.openapi-generator-ignore`
protects `README.md`, `Cargo.toml`, and `src/lib.rs`, but the generated
`src/apis/` and `src/models/` files must be regenerated and then reviewed.

First run `just fmt` from the repo root — the raw generator output does not
match our rustfmt/dprint config and formatting noise drowns out the real diff.

Then go through `git diff` and:

1. **Keep** genuinely new endpoints, models, and fields.
2. **Revert** type regressions: the swagger spec is not to be trusted on
   number types. The server serializes most 64-bit integers (`amount`,
   `created_at`, `expires_at`, `dust`, …) as JSON **strings**, but the spec
   declares them `integer`, so every regeneration downgrades our
   `Option<String>` fields to `Option<i64>`/`Option<i32>`. Any field that was
   `Option<String>` before regeneration must stay `Option<String>`. (Fields
   that are 32-bit on the wire, e.g. `vout` or `depth`, are real JSON numbers
   and keep their integer types.)
3. **Fix** broken format strings: the generator emits invalid placeholder
   names in `src/apis/indexer_service_api.rs` (dotted names like
   `{batch_outpoint.txid}` and camelCase named arguments like `assetId =`).
   Rename them to matching snake_case identifiers.
4. **Delete** any junk the generator recreates that we intentionally removed
   (check `git status` for untracked files).

Then update the hand-written wrapper (`ark-rest/src/client.rs` etc.) for any
API changes and make it compile:

```bash
cargo build -p ark-rest
cargo test -p ark-rest
```

## 4. Bump the version pins

- `.env.regtest`: point `ARKD_IMAGE` and `ARKD_WALLET_IMAGE` at the new
  release tag (this is what CI's e2e stack runs against). Check that the tag
  exists on ghcr for both images first.
- `ark-core/src/server.rs`: bump `TARGET_ARKD_VERSION` to the new arkd
  version (sent as the `X-Build-Version` header).

## 5. Finish up

- Build the workspace and run clippy/fmt via the top-level `justfile`.
- Run the e2e tests against the new arkd version if the API changed
  meaningfully.
- Mention the arkd version you generated against in the commit message.
