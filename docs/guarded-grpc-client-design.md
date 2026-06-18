# Guarded gRPC Client Design

## Problem

`ark_grpc::Client` wraps generated tonic clients for Ark and Indexer RPCs. Most RPCs need digest-mismatch handling:

1. run the requested RPC,
2. if arkd rejects it because the cached `/info` digest is stale, fetch fresh `/info`,
3. update the digest header and the higher-level client state via the refresh hook,
4. return `ServerInfoChanged` without retrying the original operation.

The current pattern relies on each method remembering to wrap the raw generated client call in `guarded(...)`. That is easy to forget when adding a new RPC method.

## Goal

Make guarded execution the normal and obvious path for every RPC, while keeping the unguarded path available only for `/info` bootstrap/refresh.

This design should:

- prevent accidental direct use of raw generated tonic clients in normal RPC methods,
- keep the existing no-automatic-retry behavior,
- keep the implementation straightforward and local to `ark-grpc`,
- avoid exposing internal refresh hooks or unguarded methods as public API.

## Design

Introduce wrapper newtypes for the generated tonic clients:

- `guarded::Ark`
- `guarded::Indexer`

`ark_grpc::Client` stores these wrappers instead of exposing or using raw generated clients directly.

```rust
pub struct Client {
    url: String,
    ark: Option<guarded::Ark>,
    indexer: Option<guarded::Indexer>,
    shared: SharedState,
}
```

The wrappers live in a child module and keep the generated tonic clients private:

```rust
mod guarded {
    use super::*;

    #[derive(Clone)]
    pub(super) struct Ark {
        raw: ArkServiceClient<InterceptedChannel>,
        shared: SharedState,
    }

    #[derive(Clone)]
    pub(super) struct Indexer {
        raw: IndexerServiceClient<InterceptedChannel>,
        info_client: ArkServiceClient<InterceptedChannel>,
        shared: SharedState,
    }
}
```

Normal RPC execution goes through `request(...)` on the wrapper. The closure receives a cloned generated client, but the wrapper always runs the returned future through the shared guard.

```rust
impl guarded::Ark {
    pub(super) async fn request<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(ArkServiceClient<InterceptedChannel>) -> Fut,
        Fut: Future<Output = Result<T, tonic::Status>>,
    {
        let client = self.raw.clone();
        let info_client = self.raw.clone();

        self.shared
            .guarded(info_client, async move {
                f(client).await.map_err(Error::request)
            })
            .await
    }

    pub(super) async fn get_info(&self) -> Result<Info, Error> {
        self.shared.get_info_unguarded(self.raw.clone()).await
    }
}

impl guarded::Indexer {
    pub(super) async fn request<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(IndexerServiceClient<InterceptedChannel>) -> Fut,
        Fut: Future<Output = Result<T, tonic::Status>>,
    {
        let client = self.raw.clone();
        let info_client = self.info_client.clone();

        self.shared
            .guarded(info_client, async move {
                f(client).await.map_err(Error::request)
            })
            .await
    }
}
```

`Indexer` also stores an Ark service client (`info_client`) because digest refresh requires calling `GetInfo`, even when the failed operation was an Indexer RPC.

## Shared state

Move digest header state and the refresh hook into a shared state object cloned by both wrappers.

```rust
#[derive(Clone, Default)]
struct SharedState {
    headers: HeaderState,
    info_refresh_hook: Arc<RwLock<Option<InfoRefreshHook>>>,
}
```

The shared guard owns the digest-mismatch behavior:

```rust
impl SharedState {
    async fn guarded<T>(
        &self,
        info_client: ArkServiceClient<InterceptedChannel>,
        op: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        match op.await {
            Ok(value) => Ok(value),
            Err(err) if err.is_digest_mismatch() => {
                let original = err;
                let info = self.fetch_info_unguarded(info_client).await?;
                let digest = info.digest.clone();

                if let Some(hook) = self.info_refresh_hook() {
                    hook(info).map_err(Error::conversion)?;
                }

                self.headers.set_digest(digest);
                Err(Error::server_info_changed(original))
            }
            Err(err) => Err(err),
        }
    }

    fn info_refresh_hook(&self) -> Option<InfoRefreshHook> {
        match self.info_refresh_hook.read() {
            Ok(hook) => hook.clone(),
            Err(poisoned) => {
                log::warn!("info refresh hook lock poisoned while reading; recovering");
                poisoned.into_inner().clone()
            }
        }
    }

    async fn get_info_unguarded(
        &self,
        client: ArkServiceClient<InterceptedChannel>,
    ) -> Result<Info, Error> {
        let info = self.fetch_info_unguarded(client).await?;
        self.headers.set_digest(info.digest.clone());
        Ok(info)
    }

    async fn fetch_info_unguarded(
        &self,
        mut client: ArkServiceClient<InterceptedChannel>,
    ) -> Result<Info, Error> {
        let response = client
            .get_info(GetInfoRequest {})
            .await
            .map_err(Error::request)?;

        response.into_inner().try_into()
    }
}
```

Implementation detail: pass the Ark service client used for refresh into `guarded(...)`. `guarded::Ark` passes a clone of its Ark client; `guarded::Indexer` passes its dedicated `info_client`. This keeps the refresh path explicit and avoids ambiguity when an Indexer RPC fails. During digest-mismatch refresh, fetch `/info` without committing the digest header, run the refresh hook, and only then update the digest header. If the hook fails, the header remains stale along with the higher-level client state, so the next request will still refresh rather than sending a fresh digest with stale local state. The refresh hook accessor should recover from a poisoned lock (or return an error) rather than silently skipping the hook. The important invariant is that refresh uses only the unguarded `/info` path and does not recursively go through the guarded request path.

## Example method after refactor

Before:

```rust
let mut client = self.indexer_client()?;

let response = self
    .guarded(async {
        client
            .get_asset(request)
            .await
            .map_err(Error::request)
    })
    .await?;
```

After:

```rust
let response = self
    .indexer()?
    .request(|mut client| async move {
        client.get_asset(request).await
    })
    .await?;
```

The public method no longer needs to remember `guarded(...)`; it can only obtain a guarded wrapper.

## How this prevents skipping the guard

The design changes the local API shape:

1. Raw generated clients are fields inside `guarded::Ark` and `guarded::Indexer`.
2. Normal `ark_grpc::Client` methods receive only wrapper values via `ark()?` / `indexer()?`.
3. The wrapper exposes `request(...)` as the normal RPC escape hatch.
4. `request(...)` always calls `SharedState::guarded(...)` with the Ark client used for digest refresh.
5. The only unguarded path is `get_info_unguarded(...)`, used for bootstrap/refresh.

This makes skipping the guard much harder to do accidentally. Adding a new RPC should follow one recognizable pattern: choose `ark()?.request(...)` or `indexer()?.request(...)`.

## Clippy guardrail

Add a lint rule to catch accidental direct calls to raw client accessors if any remain during migration:

```toml
disallowed-methods = [
  { path = "ark_grpc::client::Client::ark_client", reason = "use guarded::Ark::request so digest mismatches are guarded" },
  { path = "ark_grpc::client::Client::indexer_client", reason = "use guarded::Indexer::request so digest mismatches are guarded" },
]
```

If those accessors are removed entirely after the wrapper refactor, this lint becomes unnecessary. If they remain as private helpers, allow the lint only at intentional bootstrap/wrapper construction sites.

## Invariants

- `GetInfo` bootstrap/refresh is the only unguarded RPC.
- All Ark service RPCs, except `GetInfo`, go through `guarded::Ark::request`.
- All Indexer service RPCs go through `guarded::Indexer::request`.
- `request(...)` closures are an escape hatch over the full generated client. They should perform exactly one non-`GetInfo` RPC and propagate errors; they must not catch errors and return fallback success values.
- The closure rule above is review-enforced. If stronger compile-time enforcement becomes necessary, replace `request(...)` with concrete wrapper methods or narrower service-specific traits.
- Digest mismatch refresh updates both the request header state and the high-level client state via the refresh hook; the digest header is committed only after the hook succeeds.
- The original failed operation is not retried automatically.

## Tradeoffs

Benefits:

- Reduces the chance of forgetting digest guarding on new RPCs.
- Keeps digest refresh behavior centralized.
- Avoids adding many concrete wrapper methods.
- Keeps the generated tonic clients out of normal `Client` method bodies.

Costs:

- Adds a small wrapper layer around generated clients.
- Uses generic closures, so reviewers should still check that closures do not call `GetInfo`, perform multiple RPCs, or swallow errors.
- Requires careful handling of the unguarded `/info` path to avoid recursive refresh behavior.
