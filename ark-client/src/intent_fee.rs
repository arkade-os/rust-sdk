//! Client-side pricing of the Arkade operator's intent fee.
//!
//! The fee an intent pays is `sum(inputs) - sum(outputs)`. The operator evaluates its fee
//! programs over every input and output of a registered intent and rejects it with
//! `INTENT_INSUFFICIENT_FEE` when that difference falls short. Every batch path that builds its
//! own outputs must therefore leave the fee unclaimed:
//!
//! - an offchain input fee per VTXO (and per ArkNote);
//! - an onchain input fee per boarding output;
//! - an offchain output fee per VTXO output (settled output, change, delegator fee);
//! - an onchain output fee per collaborative-exit output.
//!
//! The fee always comes out of the settled output or the change, never out of an amount the
//! caller asked to send.

use crate::Error;
use ark_core::batch;
use ark_core::intent;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkNote;
use bitcoin::Amount;
use bitcoin::OutPoint;

/// 2025-01-01T00:00:00Z.
///
/// The operator reports `expires_at` as a Unix timestamp, or as a block height for block-based
/// batch expiries. Anything below this is taken to be a height, which the fee programs' `expiry`
/// variable cannot express, so it is left unset.
const EXPIRY_MIN_PLAUSIBLE_SECS: i64 = 1_735_689_600;

/// The [`ark_fees::OffchainInput`] fields that come from the VTXO itself.
///
/// A swept VTXO is priced as `recoverable`, anything else as `vtxo`. `birth` and `expiry` are Unix
/// timestamps in seconds; an expiry that is really a block height is omitted.
pub fn offchain_input_fee_params(vtxo: &VirtualTxOutPoint) -> ark_fees::OffchainInput {
    let input_type = if vtxo.is_swept {
        ark_fees::VtxoType::Recoverable
    } else {
        ark_fees::VtxoType::Vtxo
    };

    let expiry = (vtxo.expires_at >= EXPIRY_MIN_PLAUSIBLE_SECS).then_some(vtxo.expires_at);

    ark_fees::OffchainInput {
        amount: vtxo.amount.to_sat(),
        expiry,
        birth: Some(vtxo.created_at),
        input_type,
        weight: 0.0,
    }
}

/// The [`ark_fees::OffchainInput`] for redeeming an ArkNote: priced as a `note`, with neither
/// birth nor expiry.
pub fn note_fee_params(note: &ArkNote) -> ark_fees::OffchainInput {
    ark_fees::OffchainInput {
        amount: note.value().to_sat(),
        expiry: None,
        birth: None,
        input_type: ark_fees::VtxoType::Note,
        weight: 0.0,
    }
}

/// The [`ark_fees::OnchainInput`] for a boarding output worth `amount`.
pub fn onchain_input_fee_params(amount: Amount) -> ark_fees::OnchainInput {
    ark_fees::OnchainInput {
        amount: amount.to_sat(),
    }
}

/// Round a fee up to whole satoshis.
pub(crate) fn fee_to_amount(fee: ark_fees::FeeAmount) -> Amount {
    Amount::from_sat(fee.to_satoshis())
}

/// An input that can be priced against the intent-fee programs.
pub(crate) trait FeeInput {
    fn outpoint(&self) -> OutPoint;
    fn amount(&self) -> Amount;
}

impl FeeInput for intent::Input {
    fn outpoint(&self) -> OutPoint {
        intent::Input::outpoint(self)
    }

    fn amount(&self) -> Amount {
        intent::Input::amount(self)
    }
}

impl FeeInput for batch::OnChainInput {
    fn outpoint(&self) -> OutPoint {
        batch::OnChainInput::outpoint(self)
    }

    fn amount(&self) -> Amount {
        batch::OnChainInput::amount(self)
    }
}

/// An input together with the fee the operator charges to spend it.
#[derive(Debug, Clone)]
pub(crate) struct Priced<T> {
    pub input: T,
    pub fee: Amount,
}

impl<T: FeeInput> Priced<T> {
    /// Whether spending this input yields anything after its own fee.
    fn is_economic(&self) -> bool {
        self.fee < self.input.amount()
    }

    /// What this input contributes to the outputs once its fee is paid.
    fn net(&self) -> Amount {
        self.input
            .amount()
            .checked_sub(self.fee)
            .unwrap_or(Amount::ZERO)
    }
}

/// What to do with an input whose fee meets or exceeds its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UneconomicInputs {
    /// Drop it: the inputs were picked by the SDK.
    Skip,
    /// Fail: the caller named the inputs, and silently spending a subset of them would be
    /// surprising.
    Reject,
}

/// Boarding and VTXO inputs for a batch, each with its intent fee.
#[derive(Debug, Clone, Default)]
pub(crate) struct PricedInputs {
    pub boarding: Vec<Priced<batch::OnChainInput>>,
    pub vtxos: Vec<Priced<intent::Input>>,
}

impl PricedInputs {
    /// Price boarding inputs and VTXO inputs (paired with their fee parameters).
    ///
    /// With `at`, the offchain programs see `now()` as that Unix timestamp instead of the wall
    /// clock.
    pub fn price(
        estimator: &ark_fees::Estimator,
        boarding: Vec<batch::OnChainInput>,
        vtxos: Vec<(intent::Input, ark_fees::OffchainInput)>,
        at: Option<i64>,
    ) -> Result<Self, Error> {
        let boarding = boarding
            .into_iter()
            .map(|input| {
                let fee = estimator
                    .eval_onchain_input(onchain_input_fee_params(input.amount()))
                    .map_err(Error::ad_hoc)?;
                Ok(Priced {
                    input,
                    fee: fee_to_amount(fee),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let vtxos = vtxos
            .into_iter()
            .map(|(input, params)| {
                let fee = match at {
                    Some(at) => estimator.eval_offchain_input_at(params, at),
                    None => estimator.eval_offchain_input(params),
                }
                .map_err(Error::ad_hoc)?;
                Ok(Priced {
                    input,
                    fee: fee_to_amount(fee),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(Self { boarding, vtxos })
    }

    pub fn is_empty(&self) -> bool {
        self.boarding.is_empty() && self.vtxos.is_empty()
    }

    /// Keep only the boarding inputs in `boarding` and the VTXO inputs in `vtxos`.
    pub fn retain_outpoints(&mut self, boarding: &[OutPoint], vtxos: &[OutPoint]) {
        self.boarding
            .retain(|p| boarding.contains(&p.input.outpoint()));
        self.vtxos.retain(|p| vtxos.contains(&p.input.outpoint()));
    }

    /// Apply `policy` to inputs that cost at least as much to spend as they are worth.
    pub fn retain_economic(&mut self, policy: UneconomicInputs) -> Result<(), Error> {
        retain_economic(&mut self.boarding, policy)?;
        retain_economic(&mut self.vtxos, policy)
    }

    /// The sum of what every input contributes after its own fee.
    pub fn net_total(&self) -> Amount {
        net_total(&self.boarding) + net_total(&self.vtxos)
    }

    /// The sum of every input's fee.
    pub fn total_fee(&self) -> Amount {
        self.boarding
            .iter()
            .map(|p| p.fee)
            .chain(self.vtxos.iter().map(|p| p.fee))
            .fold(Amount::ZERO, |acc, fee| acc + fee)
    }

    pub fn into_inputs(self) -> (Vec<batch::OnChainInput>, Vec<intent::Input>) {
        (
            self.boarding.into_iter().map(|p| p.input).collect(),
            self.vtxos.into_iter().map(|p| p.input).collect(),
        )
    }
}

fn retain_economic<T: FeeInput>(
    inputs: &mut Vec<Priced<T>>,
    policy: UneconomicInputs,
) -> Result<(), Error> {
    if policy == UneconomicInputs::Reject {
        if let Some(p) = inputs.iter().find(|p| !p.is_economic()) {
            return Err(Error::coin_select(format!(
                "input {} costs {} to spend and is worth {}: drop it from the selection",
                p.input.outpoint(),
                p.fee,
                p.input.amount()
            )));
        }
    }

    inputs.retain(|p| {
        let keep = p.is_economic();
        if !keep {
            tracing::warn!(
                outpoint = %p.input.outpoint(),
                amount = %p.input.amount(),
                fee = %p.fee,
                "Skipping input that costs more in intent fees than it is worth"
            );
        }
        keep
    });

    Ok(())
}

fn net_total<T: FeeInput>(inputs: &[Priced<T>]) -> Amount {
    inputs.iter().fold(Amount::ZERO, |acc, p| acc + p.net())
}

/// The amount of the single output that settles `subtotal`, after that output's own fee.
///
/// The output fee is evaluated on the pre-deduction `subtotal`, matching the TS SDK. For a fee that
/// grows with the amount this overpays by `fee(subtotal) - fee(subtotal - fee(subtotal))`, which
/// is deliberate: solving the fixed point exactly risks landing one sat under what the operator
/// requires.
///
/// Fails if the fee consumes the subtotal or leaves less than `dust`.
pub(crate) fn settlement_output_amount(
    subtotal: Amount,
    dust: Amount,
    output_fee: impl FnOnce(Amount) -> Result<Amount, Error>,
) -> Result<Amount, Error> {
    let fee = output_fee(subtotal)?;
    let amount = subtotal.checked_sub(fee).ok_or_else(|| {
        Error::coin_select(format!("cannot settle {subtotal}: the output fee is {fee}"))
    })?;

    if amount < dust {
        return Err(Error::coin_select(format!(
            "cannot settle into sub-dust VTXO: {amount} < {dust} \
             ({subtotal} after input fees, minus a {fee} output fee)"
        )));
    }

    Ok(amount)
}

/// The change output funded by `left`, net of the fee charged on the change output itself.
///
/// The fee depends on the change amount and the change amount on the fee, so a single subtraction
/// is only right for a flat fee. This returns the largest change `c` with `c + fee(c) <= left`,
/// found by bisection.
///
/// The TS SDK iterates `c = left - fee(c)` to a fixed point instead, which never settles under a
/// proportional program with rounded-up fees (e.g. 1% of 10_000 oscillates between 9_900 and
/// 9_901); bisection always settles, and only ever returns an amount it has checked.
///
/// Returns zero, meaning "no change output", when there is nothing left or when the fee outruns
/// the change; in the latter case the leftover is paid to the operator as fee.
///
/// Fails if the change would be positive but below `dust`.
pub(crate) fn change_after_output_fee(
    left: Amount,
    dust: Amount,
    fee_on: impl Fn(Amount) -> Result<Amount, Error>,
) -> Result<Amount, Error> {
    if left == Amount::ZERO {
        return Ok(Amount::ZERO);
    }

    // Invariant: `lo` is affordable (zero trivially: no output), everything above `hi` is not.
    let (mut lo, mut hi) = (0u64, left.to_sat());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let fee = fee_on(Amount::from_sat(mid))?;
        if mid
            .checked_add(fee.to_sat())
            .is_some_and(|total| total <= left.to_sat())
        {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let change = Amount::from_sat(lo);

    if change == Amount::ZERO {
        tracing::warn!(
            %left,
            "Change does not cover its own output fee, paying it to the operator"
        );
        return Ok(Amount::ZERO);
    }

    if change < dust {
        return Err(Error::coin_select(format!(
            "cannot settle with sub-dust change VTXO: {change} < {dust}"
        )));
    }

    Ok(change)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::ScriptBuf;
    use bitcoin::Txid;

    fn estimator(config: ark_fees::Config) -> ark_fees::Estimator {
        ark_fees::Estimator::new(config).unwrap()
    }

    fn vtxo(amount: u64, created_at: i64, expires_at: i64, is_swept: bool) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            outpoint: OutPoint::new(Txid::all_zeros(), 0),
            created_at,
            expires_at,
            amount: Amount::from_sat(amount),
            script: ScriptBuf::new(),
            is_preconfirmed: false,
            is_swept,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 0,
        }
    }

    fn sats(fee: Result<ark_fees::FeeAmount, ark_fees::Error>) -> u64 {
        fee.unwrap().to_satoshis()
    }

    #[test]
    fn offchain_params_map_vtxo_fields() {
        let params = offchain_input_fee_params(&vtxo(10_000, 1_750_000_000, 1_760_000_000, false));

        assert_eq!(params.amount, 10_000);
        assert_eq!(params.input_type, ark_fees::VtxoType::Vtxo);
        assert_eq!(params.birth, Some(1_750_000_000));
        assert_eq!(params.expiry, Some(1_760_000_000));
        assert_eq!(params.weight, 0.0);
    }

    #[test]
    fn swept_vtxo_is_recoverable() {
        let params = offchain_input_fee_params(&vtxo(10_000, 1_750_000_000, 1_760_000_000, true));

        assert_eq!(params.input_type, ark_fees::VtxoType::Recoverable);
    }

    #[test]
    fn block_height_expiry_is_omitted() {
        let params = offchain_input_fee_params(&vtxo(10_000, 1_750_000_000, 850_000, false));

        assert_eq!(params.expiry, None);
    }

    #[test]
    fn note_params_are_typed_note() {
        let note = ArkNote::new([7; 32], Amount::from_sat(5_000)).unwrap();
        let params = note_fee_params(&note);

        assert_eq!(params.amount, 5_000);
        assert_eq!(params.input_type, ark_fees::VtxoType::Note);
        assert_eq!(params.birth, None);
        assert_eq!(params.expiry, None);
    }

    #[test]
    fn programs_see_mapped_fields() {
        let estimator = estimator(ark_fees::Config {
            // Young coins (under an hour old at `now()`) pay 1%, recoverable ones a flat 50,
            // notes nothing, and anything expiring within a day 10.
            intent_offchain_input_program: "inputType == 'note' ? 0.0 : \
                 inputType == 'recoverable' ? 50.0 : \
                 now() - birth < 3600.0 ? amount * 0.01 : \
                 expiry - now() < 86400.0 ? 10.0 : 0.0"
                .to_string(),
            ..Default::default()
        });

        let now = 1_800_000_000;
        let young = offchain_input_fee_params(&vtxo(10_000, now - 60, now + 1_000_000, false));
        let old = offchain_input_fee_params(&vtxo(10_000, now - 7_200, now + 1_000_000, false));
        let expiring = offchain_input_fee_params(&vtxo(10_000, now - 7_200, now + 600, false));
        let swept = offchain_input_fee_params(&vtxo(10_000, now - 60, now - 10, true));
        let note = note_fee_params(&ArkNote::new([1; 32], Amount::from_sat(10_000)).unwrap());

        assert_eq!(sats(estimator.eval_offchain_input_at(young, now)), 100);
        assert_eq!(sats(estimator.eval_offchain_input_at(old, now)), 0);
        assert_eq!(sats(estimator.eval_offchain_input_at(expiring, now)), 10);
        assert_eq!(sats(estimator.eval_offchain_input_at(swept, now)), 50);
        assert_eq!(sats(estimator.eval_offchain_input_at(note, now)), 0);
    }

    #[test]
    fn fee_rounds_up() {
        assert_eq!(fee_to_amount(ark_fees::FeeAmount(0.1)), Amount::from_sat(1));
        assert_eq!(fee_to_amount(ark_fees::FeeAmount(-3.0)), Amount::ZERO);
    }

    fn flat(fee: u64) -> impl Fn(Amount) -> Result<Amount, Error> {
        move |_| Ok(Amount::from_sat(fee))
    }

    fn percent(pct: u64) -> impl Fn(Amount) -> Result<Amount, Error> {
        move |amount| Ok(Amount::from_sat((amount.to_sat() * pct).div_ceil(100)))
    }

    #[test]
    fn settlement_output_deducts_fee_on_pre_fee_amount() {
        let dust = Amount::from_sat(330);

        let amount = settlement_output_amount(Amount::from_sat(10_000), dust, percent(1)).unwrap();

        // 1% of the 10_000 subtotal, not of the 9_900 output.
        assert_eq!(amount, Amount::from_sat(9_900));
    }

    #[test]
    fn settlement_output_without_fee_is_subtotal() {
        let amount =
            settlement_output_amount(Amount::from_sat(10_000), Amount::from_sat(330), flat(0))
                .unwrap();

        assert_eq!(amount, Amount::from_sat(10_000));
    }

    #[test]
    fn settlement_output_below_dust_is_rejected() {
        let err = settlement_output_amount(Amount::from_sat(500), Amount::from_sat(330), flat(200))
            .unwrap_err();

        assert!(err.to_string().contains("sub-dust"), "{err}");
    }

    #[test]
    fn settlement_output_fee_above_subtotal_is_rejected() {
        settlement_output_amount(Amount::from_sat(100), Amount::from_sat(330), flat(200))
            .unwrap_err();
    }

    #[test]
    fn change_settles_against_its_own_fee() {
        let dust = Amount::from_sat(330);

        // Flat: one subtraction.
        let change = change_after_output_fee(Amount::from_sat(5_000), dust, flat(100)).unwrap();
        assert_eq!(change, Amount::from_sat(4_900));

        // Proportional: the fee is charged on the change itself. The largest affordable change
        // is 9_090 (+ 909 fee); 9_091 would cost 910 and overshoot.
        let change = change_after_output_fee(Amount::from_sat(10_000), dust, percent(10)).unwrap();
        assert_eq!(change, Amount::from_sat(9_090));

        // 1% rounded up has no fixed point (9_900 -> 9_901 -> 9_900 ...); still settles.
        let change = change_after_output_fee(Amount::from_sat(10_000), dust, percent(1)).unwrap();
        assert_eq!(change, Amount::from_sat(9_900));
    }

    #[test]
    fn change_never_underpays_its_fee() {
        let dust = Amount::from_sat(330);
        let fee_on = |amount: Amount| {
            // Stepped schedule: 50 sats up to 5_000, 1% + 20 above.
            let sat = amount.to_sat();
            Ok(Amount::from_sat(if sat <= 5_000 {
                50
            } else {
                sat.div_ceil(100) + 20
            }))
        };

        for left in [400, 1_000, 5_049, 5_050, 5_100, 7_777, 123_456] {
            let left = Amount::from_sat(left);
            let change = change_after_output_fee(left, dust, fee_on).unwrap();
            let fee = fee_on(change).unwrap();
            assert!(change + fee <= left, "{change} + {fee} > {left}");
            // And no larger change would have been affordable.
            let bigger = change + Amount::ONE_SAT;
            assert!(
                bigger + fee_on(bigger).unwrap() > left,
                "{bigger} fits in {left}"
            );
        }
    }

    #[test]
    fn change_is_zero_when_nothing_is_left() {
        let change =
            change_after_output_fee(Amount::ZERO, Amount::from_sat(330), flat(100)).unwrap();

        assert_eq!(change, Amount::ZERO);
    }

    #[test]
    fn change_is_dropped_when_fee_outruns_it() {
        let change =
            change_after_output_fee(Amount::from_sat(50), Amount::from_sat(330), flat(100))
                .unwrap();

        assert_eq!(change, Amount::ZERO);
    }

    #[test]
    fn sub_dust_change_is_rejected() {
        let err = change_after_output_fee(Amount::from_sat(400), Amount::from_sat(330), flat(100))
            .unwrap_err();

        assert!(err.to_string().contains("sub-dust"), "{err}");
    }

    #[test]
    fn sub_dust_change_without_fee_is_still_rejected() {
        change_after_output_fee(Amount::from_sat(100), Amount::from_sat(330), flat(0)).unwrap_err();
    }

    /// An intent input worth `amount`, with a distinct outpoint per `seed`.
    fn intent_input(seed: u8, amount: u64) -> intent::Input {
        ArkNote::new([seed; 32], Amount::from_sat(amount))
            .unwrap()
            .to_intent_input()
            .unwrap()
    }

    /// A boarding input worth `amount`, with a distinct outpoint per `seed`.
    fn boarding_input(seed: u8, amount: u64) -> batch::OnChainInput {
        let template = intent_input(seed, amount);
        let owner_pk = bitcoin::XOnlyPublicKey::from_slice(&[
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ])
        .unwrap();
        batch::OnChainInput::new(
            bitcoin::Sequence::MAX,
            template.script_pubkey().clone(),
            template.tapscripts().to_vec(),
            template.spend_info().clone(),
            owner_pk,
            Amount::from_sat(amount),
            OutPoint::new(Txid::from_byte_array([seed; 32]), 1),
        )
    }

    fn young_vtxo_params(amount: u64, now: i64) -> ark_fees::OffchainInput {
        offchain_input_fee_params(&vtxo(amount, now - 60, now + 1_000_000, false))
    }

    fn old_vtxo_params(amount: u64, now: i64) -> ark_fees::OffchainInput {
        offchain_input_fee_params(&vtxo(amount, now - 7_200, now + 1_000_000, false))
    }

    /// Young VTXOs pay 500, boarding outputs 200, and every offchain output 1%.
    fn mutinynet_like() -> ark_fees::Estimator {
        estimator(ark_fees::Config {
            intent_offchain_input_program: "now() - birth < 3600.0 ? 500.0 : 0.0".to_string(),
            intent_onchain_input_program: "200.0".to_string(),
            intent_offchain_output_program: "amount * 0.01".to_string(),
            intent_onchain_output_program: String::new(),
        })
    }

    #[test]
    fn priced_inputs_cover_input_and_output_fees() {
        let now = 1_800_000_000;
        let estimator = mutinynet_like();

        let priced = PricedInputs::price(
            &estimator,
            vec![boarding_input(1, 20_000)],
            vec![
                (intent_input(2, 10_000), young_vtxo_params(10_000, now)),
                (intent_input(3, 10_000), old_vtxo_params(10_000, now)),
            ],
            Some(now),
        )
        .unwrap();

        assert_eq!(priced.total_fee(), Amount::from_sat(700));
        assert_eq!(priced.net_total(), Amount::from_sat(39_300));

        let output = settlement_output_amount(priced.net_total(), Amount::from_sat(330), |a| {
            Ok(fee_to_amount(
                estimator
                    .eval_offchain_output(ark_fees::Output {
                        amount: a.to_sat(),
                        script: String::new(),
                    })
                    .unwrap(),
            ))
        })
        .unwrap();

        // 40_000 in, 700 input fees, 1% of 39_300 (= 393) output fee.
        assert_eq!(output, Amount::from_sat(38_907));

        // What the operator would compute for the resulting intent is covered.
        let operator_fee = [
            estimator.eval_offchain_input_at(young_vtxo_params(10_000, now), now),
            estimator.eval_offchain_input_at(old_vtxo_params(10_000, now), now),
            estimator.eval_onchain_input(onchain_input_fee_params(Amount::from_sat(20_000))),
            estimator.eval_offchain_output_at(
                ark_fees::Output {
                    amount: output.to_sat(),
                    script: String::new(),
                },
                now,
            ),
        ]
        .into_iter()
        .map(|fee| fee_to_amount(fee.unwrap()))
        .fold(Amount::ZERO, |acc, fee| acc + fee);
        assert!(
            Amount::from_sat(40_000) - output >= operator_fee,
            "intent pays {} but operator charges {operator_fee}",
            Amount::from_sat(40_000) - output
        );
    }

    #[test]
    fn uneconomic_inputs_are_skipped_when_sdk_picked() {
        let now = 1_800_000_000;
        let mut priced = PricedInputs::price(
            &mutinynet_like(),
            vec![boarding_input(1, 200), boarding_input(2, 1_000)],
            vec![
                (intent_input(3, 400), young_vtxo_params(400, now)),
                (intent_input(4, 10_000), young_vtxo_params(10_000, now)),
            ],
            Some(now),
        )
        .unwrap();

        priced.retain_economic(UneconomicInputs::Skip).unwrap();

        assert_eq!(priced.boarding.len(), 1);
        assert_eq!(priced.boarding[0].input.amount(), Amount::from_sat(1_000));
        assert_eq!(priced.vtxos.len(), 1);
        assert_eq!(priced.vtxos[0].input.amount(), Amount::from_sat(10_000));
        assert_eq!(priced.net_total(), Amount::from_sat(800 + 9_500));
    }

    #[test]
    fn uneconomic_inputs_are_rejected_when_caller_named() {
        let now = 1_800_000_000;
        let mut priced = PricedInputs::price(
            &mutinynet_like(),
            Vec::new(),
            vec![
                (intent_input(3, 500), young_vtxo_params(500, now)),
                (intent_input(4, 10_000), young_vtxo_params(10_000, now)),
            ],
            Some(now),
        )
        .unwrap();

        let err = priced
            .retain_economic(UneconomicInputs::Reject)
            .unwrap_err();

        assert!(err.to_string().contains("costs 0.00000500 BTC"), "{err}");
    }

    #[test]
    fn retain_outpoints_filters_both_kinds() {
        let mut priced = PricedInputs::price(
            &mutinynet_like(),
            vec![boarding_input(1, 1_000), boarding_input(2, 1_000)],
            vec![
                (intent_input(3, 1_000), old_vtxo_params(1_000, 0)),
                (intent_input(4, 1_000), old_vtxo_params(1_000, 0)),
            ],
            None,
        )
        .unwrap();
        let keep = [
            priced.boarding[1].input.outpoint(),
            priced.vtxos[0].input.outpoint(),
        ];

        // The VTXO outpoint in the boarding list matches no boarding input.
        priced.retain_outpoints(&keep, &keep[1..]);

        assert_eq!(priced.boarding.len(), 1);
        assert_eq!(priced.vtxos.len(), 1);
        assert_eq!(priced.boarding[0].input.outpoint(), keep[0]);
        assert_eq!(priced.vtxos[0].input.outpoint(), keep[1]);
    }

    #[test]
    fn zero_fee_program_leaves_amounts_untouched() {
        let estimator = estimator(ark_fees::Config::default());
        let priced = PricedInputs::price(
            &estimator,
            vec![boarding_input(1, 1_000)],
            vec![(intent_input(2, 2_000), old_vtxo_params(2_000, 0))],
            None,
        )
        .unwrap();

        assert_eq!(priced.total_fee(), Amount::ZERO);
        assert_eq!(priced.net_total(), Amount::from_sat(3_000));
    }
}
