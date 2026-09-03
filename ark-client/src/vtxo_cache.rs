//! # VTXO cache
//!
//! The Ark client caches VTXOs and syncs them incrementally, instead of crawling the entire
//! (ever-growing) VTXO history on every query.
//!
//! The Ark server filters `GetVtxos` by the VTXO's `updated_at` timestamp (milliseconds), which
//! is bumped whenever a VTXO is created, spent, settled or unrolled. This lets us fetch the full
//! VTXO set once per script and afterwards only ask for the delta since the last sync.
//!
//! Two state transitions do _not_ bump `updated_at` on the server: sweeps (swept-ness is derived
//! from separate marker tables) and expiry updates. Both only matter for _unspent_ VTXOs, so the
//! sync additionally refreshes all cached unspent VTXOs by outpoint. Spent VTXOs are terminal and
//! are never refetched.
//!
//! The cache storage is pluggable via [`VtxoCacheStore`], allowing e.g. a Redis- or
//! database-backed cache shared between processes. [`InMemoryVtxoCache`] is the default.

use crate::Error;
use ark_core::server::VirtualTxOutPoint;
use async_trait::async_trait;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;

/// Subtracted from the sync watermark to compensate for clock skew between this machine and the
/// server, since the watermark is taken from the local clock but compared against server-side
/// `updated_at` timestamps.
pub(crate) const SYNC_MARGIN_MS: i64 = 5 * 60 * 1_000;

/// Storage backend for the VTXO cache.
///
/// The sync algorithm lives in the client; implementations only need to provide storage. All
/// implementations must uphold: a VTXO is keyed by its outpoint, and an upsert replaces the
/// previous entry for the same outpoint.
///
/// Implementations must be safe for concurrent use. The client serializes cache _syncs_
/// internally, but reads may happen concurrently.
#[async_trait]
pub trait VtxoCacheStore: Send + Sync {
    /// All cached VTXOs (including spent ones) belonging to any of the given scripts, most
    /// recently created first.
    async fn vtxos_for(
        &self,
        scripts: &HashSet<ScriptBuf>,
    ) -> Result<Vec<VirtualTxOutPoint>, Error>;

    /// Outpoints of cached _unspent_ VTXOs belonging to any of the given scripts. These are the
    /// VTXOs whose state can still change without bumping `updated_at` on the server.
    async fn unspent_outpoints_for(
        &self,
        scripts: &HashSet<ScriptBuf>,
    ) -> Result<Vec<OutPoint>, Error>;

    /// Insert or replace VTXOs, keyed by outpoint.
    async fn upsert(&self, vtxos: Vec<VirtualTxOutPoint>) -> Result<(), Error>;

    /// The scripts which have been fully synced at least once.
    async fn synced_scripts(&self) -> Result<HashSet<ScriptBuf>, Error>;

    /// Record that the given scripts have been fully synced.
    async fn mark_synced(&self, scripts: Vec<ScriptBuf>) -> Result<(), Error>;

    /// Local timestamp (in Unix milliseconds) at which the last sync _started_. Zero if never
    /// synced.
    async fn last_sync_ms(&self) -> Result<i64, Error>;

    /// Store the timestamp at which the current sync started.
    async fn set_last_sync_ms(&self, last_sync_ms: i64) -> Result<(), Error>;

    /// Drop all cached state, forcing the next sync to fetch everything again.
    async fn clear(&self) -> Result<(), Error>;
}

/// Default [`VtxoCacheStore`] implementation, keeping everything in memory.
#[derive(Default)]
pub struct InMemoryVtxoCache {
    state: Mutex<CacheState>,
}

impl InMemoryVtxoCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn state<T>(&self, f: impl FnOnce(&mut CacheState) -> T) -> Result<T, Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::ad_hoc("VTXO cache lock poisoned"))?;
        Ok(f(&mut state))
    }
}

#[async_trait]
impl VtxoCacheStore for InMemoryVtxoCache {
    async fn vtxos_for(
        &self,
        scripts: &HashSet<ScriptBuf>,
    ) -> Result<Vec<VirtualTxOutPoint>, Error> {
        self.state(|state| state.vtxos_for(scripts))
    }

    async fn unspent_outpoints_for(
        &self,
        scripts: &HashSet<ScriptBuf>,
    ) -> Result<Vec<OutPoint>, Error> {
        self.state(|state| state.unspent_outpoints_for(scripts))
    }

    async fn upsert(&self, vtxos: Vec<VirtualTxOutPoint>) -> Result<(), Error> {
        self.state(|state| state.upsert(vtxos))
    }

    async fn synced_scripts(&self) -> Result<HashSet<ScriptBuf>, Error> {
        self.state(|state| state.synced_scripts.clone())
    }

    async fn mark_synced(&self, scripts: Vec<ScriptBuf>) -> Result<(), Error> {
        self.state(|state| state.synced_scripts.extend(scripts))
    }

    async fn last_sync_ms(&self) -> Result<i64, Error> {
        self.state(|state| state.last_sync_ms)
    }

    async fn set_last_sync_ms(&self, last_sync_ms: i64) -> Result<(), Error> {
        self.state(|state| state.last_sync_ms = last_sync_ms)
    }

    async fn clear(&self) -> Result<(), Error> {
        self.state(|state| *state = CacheState::default())
    }
}

#[derive(Default)]
struct CacheState {
    vtxos: HashMap<OutPoint, VirtualTxOutPoint>,
    synced_scripts: HashSet<ScriptBuf>,
    last_sync_ms: i64,
}

impl CacheState {
    fn upsert(&mut self, vtxos: impl IntoIterator<Item = VirtualTxOutPoint>) {
        for vtxo in vtxos {
            self.vtxos.insert(vtxo.outpoint, vtxo);
        }
    }

    fn unspent_outpoints_for(&self, scripts: &HashSet<ScriptBuf>) -> Vec<OutPoint> {
        self.vtxos
            .values()
            .filter(|vtxo| !vtxo.is_spent && scripts.contains(&vtxo.script))
            .map(|vtxo| vtxo.outpoint)
            .collect()
    }

    fn vtxos_for(&self, scripts: &HashSet<ScriptBuf>) -> Vec<VirtualTxOutPoint> {
        let mut vtxos = self
            .vtxos
            .values()
            .filter(|vtxo| scripts.contains(&vtxo.script))
            .cloned()
            .collect::<Vec<_>>();

        vtxos.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.outpoint.cmp(&b.outpoint))
        });

        vtxos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Amount;

    fn vtxo(
        txid_byte: u8,
        script: &ScriptBuf,
        created_at: i64,
        is_spent: bool,
    ) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            outpoint: OutPoint {
                txid: bitcoin::hashes::Hash::from_byte_array([txid_byte; 32]),
                vout: 0,
            },
            created_at,
            expires_at: created_at + 1_000,
            amount: Amount::from_sat(1_000),
            script: script.clone(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 0,
        }
    }

    fn script(byte: u8) -> ScriptBuf {
        ScriptBuf::from_bytes(vec![byte; 3])
    }

    #[test]
    fn upsert_replaces_by_outpoint() {
        let mut state = CacheState::default();
        let s = script(1);

        state.upsert([vtxo(1, &s, 100, false)]);
        state.upsert([vtxo(1, &s, 100, true)]);

        let vtxos = state.vtxos_for(&HashSet::from([s]));
        assert_eq!(vtxos.len(), 1);
        assert!(vtxos[0].is_spent);
    }

    #[test]
    fn vtxos_for_filters_by_script_and_sorts_by_created_at_desc() {
        let mut state = CacheState::default();
        let s1 = script(1);
        let s2 = script(2);

        state.upsert([
            vtxo(1, &s1, 100, false),
            vtxo(2, &s1, 300, true),
            vtxo(3, &s2, 200, false),
        ]);

        let vtxos = state.vtxos_for(&HashSet::from([s1]));
        assert_eq!(vtxos.len(), 2);
        assert_eq!(vtxos[0].created_at, 300);
        assert_eq!(vtxos[1].created_at, 100);
    }

    #[test]
    fn unspent_outpoints_exclude_spent_and_foreign_scripts() {
        let mut state = CacheState::default();
        let s1 = script(1);
        let s2 = script(2);

        let unspent = vtxo(1, &s1, 100, false);
        state.upsert([
            unspent.clone(),
            vtxo(2, &s1, 200, true),
            vtxo(3, &s2, 300, false),
        ]);

        let outpoints = state.unspent_outpoints_for(&HashSet::from([s1]));
        assert_eq!(outpoints, vec![unspent.outpoint]);
    }

    #[tokio::test]
    async fn clear_resets_everything() {
        let cache = InMemoryVtxoCache::new();
        let s = script(1);

        cache.upsert(vec![vtxo(1, &s, 100, false)]).await.unwrap();
        cache.mark_synced(vec![s.clone()]).await.unwrap();
        cache.set_last_sync_ms(42).await.unwrap();

        cache.clear().await.unwrap();

        assert!(cache
            .vtxos_for(&HashSet::from([s.clone()]))
            .await
            .unwrap()
            .is_empty());
        assert!(!cache.synced_scripts().await.unwrap().contains(&s));
        assert_eq!(cache.last_sync_ms().await.unwrap(), 0);
    }
}
