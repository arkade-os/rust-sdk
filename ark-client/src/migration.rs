use crate::error::ErrorContext;
use crate::key_provider::KeyProvider;
use crate::swap_storage::SwapStorage;
use crate::utils::timeout_op;
use crate::utils::unix_now;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::server::DeprecatedSignerStatus;
use ark_core::ExplorerUtxo;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::collections::HashSet;

/// Maximum number of inputs a single deprecated-signer migration leg will settle in one batch.
///
/// A client-side safeguard: it bounds the input count of one
/// [`Client::migrate_deprecated_signer_vtxos`] leg so a wallet holding many small VTXOs does not
/// build a batch intent that exceeds the server's transaction-weight limit. Any overflow is
/// deferred to a later migration cycle (see [`MigrationLegReport::deferred`]).
pub const MAX_VTXOS_PER_SETTLEMENT: usize = 50;

/// A single VTXO or boarding output referenced in a [`DeprecatedSignerMigrationReport`].
#[derive(Debug, Clone)]
pub struct MigrationVtxoRef {
    /// The input's outpoint.
    pub outpoint: OutPoint,
    /// The input's amount.
    pub amount: Amount,
    /// The deprecated signer the input was minted under.
    pub signer_pk: XOnlyPublicKey,
    /// The signer's advertised cooperative-sign cutoff (Unix seconds); `0` means "rotate now".
    pub cutoff_date: i64,
}

/// Why a single migration leg ([`DeprecatedSignerMigrationReport::vtxo`] or
/// [`DeprecatedSignerMigrationReport::boarding`]) settled nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationSkipReason {
    /// The selected aggregate fell below the server's dust floor.
    BelowDust,
    /// Every migratable input in the leg individually exceeds the per-output ceiling
    /// (`vtxo_max_amount`); none can migrate cooperatively, so the leg has only `oversized`
    /// inputs and submitted nothing.
    OversizedOnly,
    /// The leg had no migratable inputs at all.
    NothingMigratable,
}

/// Outcome of one [`Client::migrate_deprecated_signer_vtxos`] leg.
///
/// Each leg owns its full sizing pipeline and reports independently — a failure or skip in one leg
/// never suppresses the other. The pipeline is:
///
/// 1. inputs whose individual amount exceeds the server's per-output ceiling (`vtxo_max_amount`)
///    are split out as [`Self::oversized`] — they can never form a `<= ceiling` output and must
///    exit unilaterally;
/// 2. the remainder is selected highest-value-first, bounded by both [`MAX_VTXOS_PER_SETTLEMENT`]
///    and a running aggregate within the ceiling — the overflow lands in [`Self::deferred`] for a
///    later cycle;
/// 3. if the selected aggregate is below the dust floor, the leg is [`Self::skipped`] and nothing
///    is submitted.
#[derive(Debug, Clone)]
pub struct MigrationLegReport {
    /// The settlement TXID, when this leg submitted a batch. `None` on skip.
    pub settle_txid: Option<Txid>,
    /// Inputs submitted in this leg's settlement; empty on skip.
    pub migrated: Vec<MigrationVtxoRef>,
    /// Migratable inputs deferred to a later cycle by this leg's count or amount caps.
    pub deferred: Vec<MigrationVtxoRef>,
    /// Inputs whose value alone exceeds the per-output ceiling; they require a unilateral exit and
    /// never migrate cooperatively.
    pub oversized: Vec<MigrationVtxoRef>,
    /// Why this leg submitted nothing; `None` when a settlement was attempted.
    pub skipped: Option<MigrationSkipReason>,
    /// The settlement error, if this leg's `settle_vtxos` call failed. Set independently of the
    /// other leg — a failure here does not prevent the other leg from running.
    pub error: Option<String>,
}

impl MigrationLegReport {
    /// A leg that submitted nothing for the given reason.
    fn skipped(reason: MigrationSkipReason) -> Self {
        Self {
            settle_txid: None,
            migrated: Vec::new(),
            deferred: Vec::new(),
            oversized: Vec::new(),
            skipped: Some(reason),
            error: None,
        }
    }
}

/// Result of a [`Client::migrate_deprecated_signer_vtxos`] pass, split into two symmetric legs:
/// a VTXO leg and a boarding leg. They are never combined into a single intent.
#[derive(Debug, Clone)]
pub struct DeprecatedSignerMigrationReport {
    /// The VTXO migration leg.
    pub vtxo: MigrationLegReport,
    /// The boarding-output migration leg.
    pub boarding: MigrationLegReport,
}

impl DeprecatedSignerMigrationReport {
    /// A report where both legs found nothing to migrate (e.g. the server advertises no
    /// deprecated signers, or the wallet holds no pre-cutoff deprecated-signer outputs).
    fn nothing_migratable() -> Self {
        Self {
            vtxo: MigrationLegReport::skipped(MigrationSkipReason::NothingMigratable),
            boarding: MigrationLegReport::skipped(MigrationSkipReason::NothingMigratable),
        }
    }

    /// Whether the wallet was rotated off a deprecated signer this pass — i.e. at least one leg
    /// submitted a settlement.
    pub fn rotated(&self) -> bool {
        self.vtxo.settle_txid.is_some() || self.boarding.settle_txid.is_some()
    }

    /// The settlement TXIDs produced this pass (at most one per leg).
    pub fn settle_txids(&self) -> Vec<Txid> {
        [self.vtxo.settle_txid, self.boarding.settle_txid]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// Outcome of sizing one migration leg's candidate inputs against the server limits, before any
/// settlement I/O. Produced by [`size_migration_leg`].
#[derive(Debug, Clone)]
struct MigrationLegSizing {
    /// Inputs chosen to be settled this pass (highest-value-first within the caps).
    selected: Vec<MigrationVtxoRef>,
    /// Migratable inputs deferred to a later cycle by the count or aggregate caps.
    deferred: Vec<MigrationVtxoRef>,
    /// Inputs whose individual value exceeds the per-output ceiling; they can never form a
    /// `<= ceiling` output and must exit unilaterally.
    oversized: Vec<MigrationVtxoRef>,
    /// Why nothing was selected, if so. `None` when [`Self::selected`] is non-empty and the leg
    /// should proceed to settle.
    skip_reason: Option<MigrationSkipReason>,
}

/// Size one migration leg's candidates against the per-output ceiling (`vtxo_max_amount`) and the
/// dust floor, without performing any settlement.
///
/// This is the pure core of [`Client::run_migration_leg`], factored out so its branching (oversized
/// split, count cap, running-aggregate ceiling, dust floor, and the skip-reason classification) is
/// unit-testable without a `Client`/network. The pipeline is:
///
/// 1. inputs whose individual amount exceeds `vtxo_max_amount` are split out as `oversized` (a
///    `None` ceiling means no limit, so nothing is oversized);
/// 2. the remainder is selected highest-value-first, bounded by both [`MAX_VTXOS_PER_SETTLEMENT`]
///    (a hard stop) and a running aggregate kept within the ceiling (a skip, so a smaller input
///    behind a larger one can still get in); the overflow lands in `deferred`;
/// 3. if nothing was selected, or the selected aggregate is below `dust`, `skip_reason` is set
///    ([`MigrationSkipReason::OversizedOnly`] when the only candidates were oversized, else
///    [`MigrationSkipReason::BelowDust`]); an empty candidate list yields
///    [`MigrationSkipReason::NothingMigratable`].
fn size_migration_leg(
    candidates: Vec<MigrationVtxoRef>,
    vtxo_max_amount: Option<Amount>,
    dust: Amount,
) -> MigrationLegSizing {
    if candidates.is_empty() {
        return MigrationLegSizing {
            selected: Vec::new(),
            deferred: Vec::new(),
            oversized: Vec::new(),
            skip_reason: Some(MigrationSkipReason::NothingMigratable),
        };
    }

    // (1) Split out inputs whose INDIVIDUAL amount exceeds the per-output ceiling. They can
    // never form a `<= ceiling` output, so they cannot migrate cooperatively and must exit
    // unilaterally. Report them rather than dropping them. `None` ceiling => no limit.
    let (oversized, mut sized): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| vtxo_max_amount.is_some_and(|max| c.amount > max));

    if !oversized.is_empty() {
        tracing::warn!(
            count = oversized.len(),
            ?vtxo_max_amount,
            "Deprecated-signer migration: inputs exceed the per-output limit and cannot be \
             migrated cooperatively; they require a unilateral exit"
        );
    }

    // (2) Select highest-value-first, bounded by both the count cap and a running aggregate
    // within the ceiling. Skipped (not stopped) on an aggregate breach so a smaller input
    // behind an oversized-but-sized one still gets in; the count cap is a hard stop. The rest
    // is deferred to a later cycle.
    sized.sort_by_key(|c| std::cmp::Reverse(c.amount));

    let mut selected: Vec<MigrationVtxoRef> = Vec::new();
    let mut deferred: Vec<MigrationVtxoRef> = Vec::new();
    let mut aggregate = Amount::ZERO;
    for candidate in sized {
        if selected.len() >= MAX_VTXOS_PER_SETTLEMENT {
            deferred.push(candidate);
            continue;
        }
        let next = aggregate + candidate.amount;
        if vtxo_max_amount.is_some_and(|max| next > max) {
            deferred.push(candidate);
            continue;
        }
        aggregate = next;
        selected.push(candidate);
    }

    // (3) A migration output equals the gross sum of its inputs (migration is fee-exempt), so a
    // selected aggregate below dust would be rejected — skip the leg.
    let skip_reason = if selected.is_empty() || aggregate < dust {
        // Nothing got selected and the only candidates were oversized => OversizedOnly;
        // otherwise the (sized) selection summed below dust.
        if selected.is_empty() && !oversized.is_empty() {
            Some(MigrationSkipReason::OversizedOnly)
        } else {
            Some(MigrationSkipReason::BelowDust)
        }
    } else {
        None
    };

    MigrationLegSizing {
        selected,
        deferred,
        oversized,
        skip_reason,
    }
}

/// Whether the wallet holds any funds under a (deprecated) signer — spendable VTXOs, recoverable
/// VTXOs, or boarding outputs — deciding whether the signer is surfaced by
/// [`Client::deprecated_signer_status`].
///
/// Recoverable VTXOs must count: an expired signer whose VTXOs have all become recoverable
/// (`spendable_count == 0`) still holds funds the user needs surfaced. Counting only spendable
/// VTXOs would drop such a signer from the report and hide those funds.
fn signer_holds_funds(
    spendable_count: usize,
    recoverable_count: usize,
    boarding_count: usize,
) -> bool {
    spendable_count + recoverable_count + boarding_count > 0
}

/// Classify a deprecated signer from its advertised cutoff and the current time, returning the
/// [`DeprecatedSignerStatus`] and `seconds_until_cutoff` hint.
///
/// Pure core of [`Client::deprecated_signer_status`], factored out so the classification is
/// unit-testable without a `Client`/network. Consistent with
/// [`ark_core::server::Info::signer_status_at`] and the `is_pre_cutoff_deprecated` check in
/// [`Client::migrate_deprecated_signer_vtxos`]: a `cutoff_date` of `0` is "rotate now"
/// ([`DeprecatedSignerStatus::DueNow`], still co-signable); a future cutoff is
/// [`DeprecatedSignerStatus::Migratable`] (with a positive `seconds_until_cutoff`); a passed cutoff
/// is [`DeprecatedSignerStatus::Expired`].
fn classify_deprecated_signer(cutoff_date: i64, now: i64) -> (DeprecatedSignerStatus, Option<i64>) {
    let status = DeprecatedSignerStatus::from_cutoff(cutoff_date, now);
    (status, status.seconds_until_cutoff(cutoff_date, now))
}

/// Read-only, per-signer status of the deprecated server signers the wallet currently holds funds
/// under. Produced by [`Client::deprecated_signer_status`].
///
/// This is observability only — building it never moves funds and never settles or migrates. The
/// `recoverable_*` vs `awaiting_sweep_*` split and `next_sweep_eta` are only populated for
/// [`DeprecatedSignerStatus::Expired`] signers (the post-cutoff recover-on-sweep lifecycle applies
/// to VTXOs only).
#[derive(Debug, Clone)]
pub struct DeprecatedSignerReport {
    /// The deprecated signer's x-only key.
    pub signer_pk: XOnlyPublicKey,
    /// The signer's status, derived from its cutoff and the current time.
    pub status: DeprecatedSignerStatus,
    /// The advertised cooperative-sign cutoff (Unix seconds); `0` means "rotate immediately".
    pub cutoff_date: i64,
    /// Seconds until the cutoff (`cutoff_date - now`); `None` when no future cutoff is advertised
    /// (i.e. `cutoff_date == 0` or already passed).
    pub seconds_until_cutoff: Option<i64>,
    /// Number of spendable (non-recoverable) VTXOs the wallet holds under this signer.
    pub vtxo_count: usize,
    /// Total value of those spendable VTXOs.
    pub vtxo_value: Amount,
    /// Number of confirmed boarding UTXOs the wallet holds under this signer (includes those whose
    /// own CSV exit window has elapsed — they leave via the unilateral sweep).
    pub boarding_count: usize,
    /// Total value of those boarding UTXOs.
    pub boarding_value: Amount,
    /// Expired-signer VTXOs already swept/expired and queued for recovery to the active signer.
    /// Non-zero only on [`DeprecatedSignerStatus::Expired`] rows.
    pub recoverable_count: usize,
    /// Total value of the recoverable VTXOs.
    pub recoverable_value: Amount,
    /// Expired-signer VTXOs not yet swept; awaiting the server batch sweep before they become
    /// recoverable. Non-zero only on [`DeprecatedSignerStatus::Expired`] rows.
    pub awaiting_sweep_count: usize,
    /// Total value of the awaiting-sweep VTXOs.
    pub awaiting_sweep_value: Amount,
    /// Soonest VTXO expiry (Unix seconds) among the awaiting-sweep set, as a recovery ETA hint.
    /// `None` when there are no awaiting-sweep VTXOs under this signer.
    pub next_sweep_eta: Option<i64>,
}

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: KeyProvider,
{
    /// Sweep VTXOs and boarding outputs minted under a *pre-cutoff* deprecated server signer to
    /// the current signer, then report what moved.
    ///
    /// Only deprecated-signer, pre-cutoff inputs are touched — current-signer outputs are left
    /// untouched (no consolidation, no incidental settlement fee), and past-cutoff outputs are
    /// skipped automatically by [`Self::fetch_commitment_transaction_inputs`] (the operator won't
    /// co-sign the old key, so they become recoverable after expiry and exit via the recovery
    /// path).
    ///
    /// Migration runs as two **independent** legs — a VTXO leg and a boarding leg — each routed
    /// through [`Self::settle_vtxos`] with its own scoped outpoint set. A failure in one leg does
    /// not suppress the other. Before settling, each leg is sized against the server's per-output
    /// ceiling (`vtxo_max_amount`) and dust floor (see [`MigrationLegReport`] for the exact
    /// pipeline): inputs that individually exceed the ceiling are reported as `oversized` (they can
    /// never form a `<= ceiling` output and must exit unilaterally — they are NOT silently
    /// dropped); the remainder is selected highest-value-first up to [`MAX_VTXOS_PER_SETTLEMENT`]
    /// and a running aggregate within the ceiling, deferring the rest to a later cycle; a leg whose
    /// selected aggregate is below dust is skipped.
    ///
    /// When the server advertises no deprecated signers, returns an empty
    /// [`MigrationSkipReason::NothingMigratable`] report without touching the wallet.
    pub async fn migrate_deprecated_signer_vtxos<R>(
        &self,
        rng: &mut R,
    ) -> Result<DeprecatedSignerMigrationReport, Error>
    where
        R: rand::Rng + rand::CryptoRng + Clone,
    {
        // Snapshot the server info once (TOCTOU): the empty-check, the per-input
        // classification closure, and the leg sizing must all see the same
        // `deprecated_signers`/`vtxo_max_amount`/`dust` even if a concurrent digest-driven
        // `refresh_server_info` swaps the snapshot mid-call.
        let server_info = self.server_info()?;
        if server_info.deprecated_signers.is_empty() {
            return Ok(DeprecatedSignerMigrationReport::nothing_migratable());
        }

        let now = unix_now()?;

        let is_pre_cutoff_deprecated = |server_pk: XOnlyPublicKey| -> Option<i64> {
            if !server_info
                .signer_status_at(server_pk, now)
                .is_pre_cutoff_deprecated()
            {
                return None;
            }

            server_info
                .deprecated_signers
                .iter()
                .find(|ds| ds.pk.x_only_public_key().0 == server_pk)
                .map(|ds| ds.cutoff_date)
        };

        // `fetch_commitment_transaction_inputs` already drops PAST-cutoff deprecated inputs (the
        // operator won't co-sign the old key). We narrow further to the PRE-cutoff deprecated
        // inputs, which is exactly the cooperatively-migratable set.
        let (boarding_inputs, vtxo_inputs, _) =
            self.fetch_commitment_transaction_inputs(now).await?;

        // The VTXO inputs only expose their script pubkey, so resolve each one's signer via the
        // script -> VTXO map (the same mapping `offchain_balance`/`settle_at` rely on).
        let (_, script_map) = self.list_vtxos().await?;

        // Build the candidate (outpoint, amount, signer, cutoff) list for the VTXO leg.
        let mut vtxo_candidates: Vec<MigrationVtxoRef> = Vec::new();
        for input in &vtxo_inputs {
            let Some(vtxo) = script_map.get(input.script_pubkey()) else {
                tracing::debug!(
                    outpoint = %input.outpoint(),
                    "Skipping VTXO with no spend info during migration"
                );
                continue;
            };
            if let Some(cutoff_date) = is_pre_cutoff_deprecated(vtxo.server_pk()) {
                vtxo_candidates.push(MigrationVtxoRef {
                    outpoint: input.outpoint(),
                    amount: input.amount(),
                    signer_pk: vtxo.server_pk(),
                    cutoff_date,
                });
            }
        }

        // Build the candidate list for the boarding leg.
        let mut boarding_candidates: Vec<MigrationVtxoRef> = Vec::new();
        for input in &boarding_inputs {
            let signer_pk = input.boarding_output().server_pk();
            if let Some(cutoff_date) = is_pre_cutoff_deprecated(signer_pk) {
                boarding_candidates.push(MigrationVtxoRef {
                    outpoint: input.outpoint(),
                    amount: input.amount(),
                    signer_pk,
                    cutoff_date,
                });
            }
        }

        if vtxo_candidates.is_empty() && boarding_candidates.is_empty() {
            tracing::debug!("No migratable deprecated-signer VTXOs or boarding outputs found");
            return Ok(DeprecatedSignerMigrationReport::nothing_migratable());
        }

        tracing::info!(
            num_vtxos = vtxo_candidates.len(),
            num_boarding = boarding_candidates.len(),
            "Found pre-cutoff deprecated-signer outputs; migrating to current signer"
        );

        let vtxo_max_amount = server_info.vtxo_max_amount;
        let dust = server_info.dust;

        // Run each leg independently so a failure in one does not suppress the other.
        let vtxo_leg = self
            .run_migration_leg(rng, vtxo_candidates, vtxo_max_amount, dust, true)
            .await?;
        let boarding_leg = self
            .run_migration_leg(rng, boarding_candidates, vtxo_max_amount, dust, false)
            .await?;

        Ok(DeprecatedSignerMigrationReport {
            vtxo: vtxo_leg,
            boarding: boarding_leg,
        })
    }

    /// Report the per-signer status of every deprecated server signer the wallet currently holds
    /// funds under, without migrating anything.
    ///
    /// This is observability only — it never moves funds and never calls settle or migrate. It is
    /// the read-only sibling of [`Self::migrate_deprecated_signer_vtxos`]. For each deprecated
    /// signer it merges the wallet's VTXO holdings (resolved via the script -> VTXO map, like
    /// [`Self::offchain_balance`]) and its on-chain boarding holdings (grouped by
    /// [`BoardingOutput::server_pk`]) into one [`DeprecatedSignerReport`].
    ///
    /// Signers under which the wallet holds neither VTXOs nor boarding outputs are omitted. When
    /// the server advertises no deprecated signers, returns an empty vector without touching the
    /// chain.
    ///
    /// For [`DeprecatedSignerStatus::Expired`] signers the VTXOs are additionally split into the
    /// already-swept/expired `recoverable_*` set and the not-yet-swept `awaiting_sweep_*` set, and
    /// `next_sweep_eta` is the soonest VTXO expiry (`expires_at`) among the awaiting set.
    pub async fn deprecated_signer_status(&self) -> Result<Vec<DeprecatedSignerReport>, Error> {
        // Snapshot once (TOCTOU): the empty-check and every per-signer classification must see the
        // same `deprecated_signers`/`dust` even if a concurrent refresh swaps the snapshot.
        let server_info = self.server_info()?;
        if server_info.deprecated_signers.is_empty() {
            return Ok(Vec::new());
        }

        let now = unix_now()?;
        let dust = server_info.dust;

        // Aggregate VTXO holdings per signer in a single pass over all unspent VTXOs, resolving the
        // signer via the script -> VTXO map (the same mapping `offchain_balance` relies on).
        #[derive(Default)]
        struct VtxoAgg {
            // Spendable (non-recoverable) VTXOs.
            spendable_count: usize,
            spendable_value: Amount,
            // Already-swept/expired VTXOs (only surfaced for past-cutoff signers).
            recoverable_count: usize,
            recoverable_value: Amount,
            // Soonest expiry among the spendable (awaiting-sweep) VTXOs.
            next_sweep_eta: Option<i64>,
        }

        let (vtxo_list, script_map) = self.list_vtxos().await.context("failed to list VTXOs")?;
        let mut vtxo_aggs: HashMap<XOnlyPublicKey, VtxoAgg> = HashMap::new();
        for v in vtxo_list.all_unspent() {
            let Some(vtxo) = script_map.get(&v.script) else {
                continue;
            };
            let agg = vtxo_aggs.entry(vtxo.server_pk()).or_default();
            if v.is_recoverable(dust) {
                agg.recoverable_count += 1;
                agg.recoverable_value += v.amount;
            } else {
                agg.spendable_count += 1;
                agg.spendable_value += v.amount;
                agg.next_sweep_eta = Some(match agg.next_sweep_eta {
                    Some(eta) => eta.min(v.expires_at),
                    None => v.expires_at,
                });
            }
        }

        // Aggregate confirmed boarding holdings per signer. Mirrors the discovery in
        // `fetch_commitment_transaction_inputs` (boarding outputs -> `find_outpoints`) but WITHOUT
        // the cutoff/CSV-claimability filters: the report counts every confirmed, unspent boarding
        // coin under a signer, including past-cutoff and CSV-expired ones (they still leave via the
        // unilateral sweep).
        let mut boarding_aggs: HashMap<XOnlyPublicKey, (usize, Amount)> = HashMap::new();
        let mut seen_outpoints = HashSet::new();
        for boarding_output in self.inner.wallet.get_boarding_outputs()? {
            let outpoints = timeout_op(
                self.inner.timeout,
                self.blockchain().find_outpoints(boarding_output.address()),
            )
            .await
            .context("failed to find boarding outpoints")??;

            for o in outpoints.iter() {
                if let ExplorerUtxo {
                    outpoint,
                    amount,
                    confirmation_blocktime: Some(_),
                    is_spent: false,
                    ..
                } = o
                {
                    if !seen_outpoints.insert(*outpoint) {
                        continue;
                    }
                    let entry = boarding_aggs
                        .entry(boarding_output.server_pk())
                        .or_insert((0, Amount::ZERO));
                    entry.0 += 1;
                    entry.1 += *amount;
                }
            }
        }

        let mut reports = Vec::new();
        for ds in &server_info.deprecated_signers {
            let signer_pk = ds.pk.x_only_public_key().0;
            let cutoff_date = ds.cutoff_date;

            // Status + `seconds_until_cutoff`, consistent with `is_signer_past_cutoff_at` /
            // `is_pre_cutoff_deprecated`: cutoff `0` = rotate-now (still co-signable); a future
            // cutoff = migratable; a passed cutoff = expired.
            let (status, seconds_until_cutoff) = classify_deprecated_signer(cutoff_date, now);

            let vtxo_agg = vtxo_aggs.get(&signer_pk);
            let (boarding_count, boarding_value) = boarding_aggs
                .get(&signer_pk)
                .copied()
                .unwrap_or((0, Amount::ZERO));

            let vtxo_count = vtxo_agg.map(|a| a.spendable_count).unwrap_or(0);
            let vtxo_value = vtxo_agg.map(|a| a.spendable_value).unwrap_or(Amount::ZERO);

            // Skip signers under which the wallet holds no funds at all.
            let recoverable_vtxo_count = vtxo_agg.map(|a| a.recoverable_count).unwrap_or(0);
            if !signer_holds_funds(vtxo_count, recoverable_vtxo_count, boarding_count) {
                continue;
            }

            // The recover-on-sweep split applies to past-cutoff (expired) signers only; for still
            // co-signable signers these stay zero / `None`.
            let is_expired = status == DeprecatedSignerStatus::Expired;
            let recoverable_count = vtxo_agg
                .filter(|_| is_expired)
                .map(|a| a.recoverable_count)
                .unwrap_or(0);
            let recoverable_value = vtxo_agg
                .filter(|_| is_expired)
                .map(|a| a.recoverable_value)
                .unwrap_or(Amount::ZERO);
            let (awaiting_sweep_count, awaiting_sweep_value, next_sweep_eta) = if is_expired {
                (
                    vtxo_count,
                    vtxo_value,
                    vtxo_agg.and_then(|a| a.next_sweep_eta),
                )
            } else {
                (0, Amount::ZERO, None)
            };

            reports.push(DeprecatedSignerReport {
                signer_pk,
                status,
                cutoff_date,
                seconds_until_cutoff,
                vtxo_count,
                vtxo_value,
                boarding_count,
                boarding_value,
                recoverable_count,
                recoverable_value,
                awaiting_sweep_count,
                awaiting_sweep_value,
                next_sweep_eta,
            });
        }

        Ok(reports)
    }

    /// Size a single migration leg against the server limits and settle the selected inputs.
    ///
    /// `is_vtxo_leg` selects which argument of [`Self::settle_vtxos`] the chosen outpoints are
    /// passed in (VTXO vs boarding);
    /// the other argument is empty so each leg is a distinct intent.
    async fn run_migration_leg<R>(
        &self,
        rng: &mut R,
        candidates: Vec<MigrationVtxoRef>,
        vtxo_max_amount: Option<Amount>,
        dust: Amount,
        is_vtxo_leg: bool,
    ) -> Result<MigrationLegReport, Error>
    where
        R: rand::Rng + rand::CryptoRng + Clone,
    {
        // Pure sizing (split oversized, cap count + aggregate, dust floor) is factored into
        // `size_migration_leg` so it can be unit-tested without a `Client`/network. This leg only
        // adds the I/O: settling the selected inputs and mapping the outcome onto a report.
        let MigrationLegSizing {
            selected,
            deferred,
            oversized,
            skip_reason,
        } = size_migration_leg(candidates, vtxo_max_amount, dust);

        if let Some(reason) = skip_reason {
            return Ok(MigrationLegReport {
                settle_txid: None,
                migrated: Vec::new(),
                // Surface any sized-but-skipped inputs (e.g. a below-dust selection) as deferred
                // so a later cycle re-attempts them, matching the settle-error path below. For
                // OversizedOnly/NothingMigratable `selected` is empty, so this is a no-op there.
                deferred: selected.into_iter().chain(deferred).collect(),
                oversized,
                skipped: Some(reason),
                error: None,
            });
        }

        let selected_outpoints: Vec<OutPoint> = selected.iter().map(|c| c.outpoint).collect();
        let settle_result = if is_vtxo_leg {
            self.settle_vtxos(rng, &selected_outpoints, &[]).await
        } else {
            self.settle_vtxos(rng, &[], &selected_outpoints).await
        };

        // Capture (rather than propagate) the settle error so the caller can still run the other
        // leg — a failure in one leg must not suppress the other.
        Ok(match settle_result {
            Ok(settle_txid) => MigrationLegReport {
                settle_txid,
                migrated: selected,
                deferred,
                oversized,
                skipped: None,
                error: None,
            },
            Err(e) => {
                tracing::warn!(error = %e, "Deprecated-signer migration leg failed to settle");
                MigrationLegReport {
                    settle_txid: None,
                    migrated: Vec::new(),
                    // The selected inputs did not move; surface them as deferred so a retry
                    // re-attempts them.
                    deferred: selected.into_iter().chain(deferred).collect(),
                    oversized,
                    skipped: None,
                    error: Some(e.to_string()),
                }
            }
        })
    }
}

/// Unit coverage for the pure deprecated-signer-migration logic: the per-leg sizing pipeline
/// ([`size_migration_leg`]), the signer classification ([`classify_deprecated_signer`]), and the
/// empty-`deprecated_signers` short-circuit report ([`DeprecatedSignerMigrationReport`]). These
/// run without a `Client`/network — they exercise the same branching the regtest e2e tests cover
/// end-to-end.
#[cfg(test)]
mod migration_tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::key::Keypair;
    use bitcoin::key::Secp256k1;

    /// A migratable candidate of the given amount. Each gets a distinct outpoint (via `vout`) so
    /// selection order and counts are observable; the signer/cutoff are fixed placeholders the
    /// sizing logic does not inspect.
    fn candidate(vout: u32, amount: Amount) -> MigrationVtxoRef {
        let secp = Secp256k1::new();
        let sk = bitcoin::secp256k1::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let signer_pk = Keypair::from_secret_key(&secp, &sk).x_only_public_key().0;
        MigrationVtxoRef {
            outpoint: OutPoint::new(Txid::from_byte_array([0u8; 32]), vout),
            amount,
            signer_pk,
            cutoff_date: 0,
        }
    }

    fn sat(n: u64) -> Amount {
        Amount::from_sat(n)
    }

    // ── size_migration_leg ───────────────────────────────────────────────────

    #[test]
    fn sizing_empty_candidates_is_nothing_migratable() {
        let sizing = size_migration_leg(Vec::new(), Some(sat(1000)), sat(330));
        assert!(sizing.selected.is_empty());
        assert!(sizing.deferred.is_empty());
        assert!(sizing.oversized.is_empty());
        assert_eq!(
            sizing.skip_reason,
            Some(MigrationSkipReason::NothingMigratable)
        );
    }

    #[test]
    fn sizing_selects_all_when_within_limits() {
        let candidates = vec![candidate(0, sat(500)), candidate(1, sat(400))];
        let sizing = size_migration_leg(candidates, Some(sat(1000)), sat(330));
        assert_eq!(sizing.selected.len(), 2);
        assert!(sizing.deferred.is_empty());
        assert!(sizing.oversized.is_empty());
        assert_eq!(sizing.skip_reason, None);
        // Highest-value-first ordering.
        assert_eq!(sizing.selected[0].amount, sat(500));
        assert_eq!(sizing.selected[1].amount, sat(400));
    }

    #[test]
    fn sizing_caps_to_vtxo_max_deferring_the_rest() {
        // Ceiling 1000: the 700 fits, the next 700 would push the aggregate to 1400 (> ceiling)
        // so it is deferred, not stopped — a later 300 still fits under the running aggregate.
        let candidates = vec![
            candidate(0, sat(700)),
            candidate(1, sat(700)),
            candidate(2, sat(300)),
        ];
        let sizing = size_migration_leg(candidates, Some(sat(1000)), sat(330));
        assert_eq!(sizing.selected.len(), 2);
        let selected: Vec<_> = sizing.selected.iter().map(|c| c.amount).collect();
        assert_eq!(selected, vec![sat(700), sat(300)]);
        assert_eq!(sizing.deferred.len(), 1);
        assert_eq!(sizing.deferred[0].amount, sat(700));
        assert!(sizing.oversized.is_empty());
        assert_eq!(sizing.skip_reason, None);
    }

    #[test]
    fn sizing_splits_oversized_inputs() {
        // 1500 alone exceeds the 1000 ceiling: it can never form a `<= ceiling` output, so it is
        // reported as oversized (not dropped, not deferred). The 600 still migrates.
        let candidates = vec![candidate(0, sat(1500)), candidate(1, sat(600))];
        let sizing = size_migration_leg(candidates, Some(sat(1000)), sat(330));
        assert_eq!(sizing.oversized.len(), 1);
        assert_eq!(sizing.oversized[0].amount, sat(1500));
        assert_eq!(sizing.selected.len(), 1);
        assert_eq!(sizing.selected[0].amount, sat(600));
        assert!(sizing.deferred.is_empty());
        assert_eq!(sizing.skip_reason, None);
    }

    #[test]
    fn sizing_oversized_only_when_all_exceed_ceiling() {
        let candidates = vec![candidate(0, sat(1500)), candidate(1, sat(2000))];
        let sizing = size_migration_leg(candidates, Some(sat(1000)), sat(330));
        assert_eq!(sizing.oversized.len(), 2);
        assert!(sizing.selected.is_empty());
        assert!(sizing.deferred.is_empty());
        assert_eq!(sizing.skip_reason, Some(MigrationSkipReason::OversizedOnly));
    }

    #[test]
    fn sizing_skips_below_dust() {
        // Selected aggregate (200) is below the dust floor (330): the leg is skipped as BelowDust
        // (no oversized inputs involved). The candidate still satisfied the per-input and aggregate
        // ceilings, so it remains in `selected`; `run_migration_leg` reads `selected` only when
        // `skip_reason` is `None`, so a BelowDust leg settles nothing.
        let candidates = vec![candidate(0, sat(200))];
        let sizing = size_migration_leg(candidates, Some(sat(1000)), sat(330));
        assert_eq!(sizing.skip_reason, Some(MigrationSkipReason::BelowDust));
        assert!(sizing.oversized.is_empty());
    }

    #[test]
    fn sizing_defers_beyond_count_cap() {
        // One more candidate than the per-settlement count cap, each tiny so the aggregate ceiling
        // never binds: exactly MAX_VTXOS_PER_SETTLEMENT are selected and the remainder is deferred.
        let candidates: Vec<_> = (0..=MAX_VTXOS_PER_SETTLEMENT as u32)
            .map(|i| candidate(i, sat(1)))
            .collect();
        // `None` ceiling => the aggregate cap does not apply; dust floor of 1 sat is met by the
        // selected aggregate (MAX_VTXOS_PER_SETTLEMENT sats).
        let sizing = size_migration_leg(candidates, None, sat(1));
        assert_eq!(sizing.selected.len(), MAX_VTXOS_PER_SETTLEMENT);
        assert_eq!(sizing.deferred.len(), 1);
        assert!(sizing.oversized.is_empty());
        assert_eq!(sizing.skip_reason, None);
    }

    #[test]
    fn sizing_none_ceiling_means_no_oversized() {
        // With no advertised ceiling, no input is ever oversized regardless of size.
        let candidates = vec![candidate(0, sat(10_000_000)), candidate(1, sat(20_000_000))];
        let sizing = size_migration_leg(candidates, None, sat(330));
        assert!(sizing.oversized.is_empty());
        assert_eq!(sizing.selected.len(), 2);
        assert_eq!(sizing.skip_reason, None);
    }

    // ── classify_deprecated_signer ───────────────────────────────────────────

    #[test]
    fn classify_cutoff_zero_is_due_now() {
        let (status, secs) = classify_deprecated_signer(0, 1_000_000);
        assert_eq!(status, DeprecatedSignerStatus::DueNow);
        assert_eq!(secs, None);
    }

    #[test]
    fn classify_future_cutoff_is_migratable() {
        let now = 1_000_000i64;
        let (status, secs) = classify_deprecated_signer(now + 86_400, now);
        assert_eq!(status, DeprecatedSignerStatus::Migratable);
        assert_eq!(secs, Some(86_400));
    }

    #[test]
    fn classify_exact_cutoff_boundary_is_expired() {
        // cutoff_date <= now (and != 0) => expired. The boundary (cutoff == now) requires
        // recovery instead of cooperative migration.
        let now = 1_000_000i64;
        let (status, secs) = classify_deprecated_signer(now, now);
        assert_eq!(status, DeprecatedSignerStatus::Expired);
        assert_eq!(secs, None);
    }

    #[test]
    fn classify_past_cutoff_is_expired() {
        let now = 1_000_000i64;
        let (status, secs) = classify_deprecated_signer(now - 1, now);
        assert_eq!(status, DeprecatedSignerStatus::Expired);
        assert_eq!(secs, None);
    }

    // ── deprecated_signer_status emptiness skip ──────────────────────────────

    #[test]
    fn signer_with_only_recoverable_vtxos_is_kept() {
        // Regression for the report skip dropping an expired signer whose VTXOs are all
        // recoverable (spendable_count == 0): those funds must still be surfaced.
        assert!(signer_holds_funds(0, 3, 0));
    }

    #[test]
    fn signer_with_only_spendable_vtxos_is_kept() {
        assert!(signer_holds_funds(5, 0, 0));
    }

    #[test]
    fn signer_with_only_boarding_is_kept() {
        assert!(signer_holds_funds(0, 0, 2));
    }

    #[test]
    fn signer_with_no_funds_is_dropped() {
        assert!(!signer_holds_funds(0, 0, 0));
    }

    // ── empty-deprecated-signers short-circuit report ────────────────────────

    #[test]
    fn nothing_migratable_report_is_not_rotated() {
        // The report `migrate_deprecated_signer_vtxos` returns when the server advertises no
        // deprecated signers: not rotated, no settle txids, both legs NothingMigratable.
        let report = DeprecatedSignerMigrationReport::nothing_migratable();
        assert!(!report.rotated());
        assert!(report.settle_txids().is_empty());
        assert_eq!(
            report.vtxo.skipped,
            Some(MigrationSkipReason::NothingMigratable)
        );
        assert_eq!(
            report.boarding.skipped,
            Some(MigrationSkipReason::NothingMigratable)
        );
        assert!(report.vtxo.migrated.is_empty());
        assert!(report.boarding.migrated.is_empty());
    }
}
