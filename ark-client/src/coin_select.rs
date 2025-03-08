use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::ExplorerUtxo;
use ark_core::unilateral_exit;
use bitcoin::Amount;
use jiff::SignedDuration;
use jiff::Timestamp;

/// Select boarding outputs and VTXOs to be used as inputs in on-chain transactions, exiting the Ark
/// ecosystem.
///
/// This function prioritizes boarding outputs over VTXOs. That is, we may not select any VTXOs if
/// the `target_amount` is covered using only boarding outputs.
///
/// TODO: We should use a coin selection algorithm that takes into account fees e.g.
/// https://github.com/bitcoindevkit/coin-select.
///
/// TODO: Part of this logic needs to be extracted into `ark-core`.
pub async fn coin_select_for_onchain<B, W>(
    client: &Client<B, W>,
    target_amount: Amount,
) -> Result<
    (
        Vec<unilateral_exit::OnChainInput>,
        Vec<unilateral_exit::VtxoInput>,
    ),
    Error,
>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    let boarding_outputs = client.inner.wallet.get_boarding_outputs()?;

    let now = Timestamp::now();

    let mut selected_boarding_outputs = Vec::new();
    let mut selected_amount = Amount::ZERO;

    for boarding_output in boarding_outputs.iter() {
        if target_amount <= selected_amount {
            return Ok((selected_boarding_outputs, Vec::new()));
        }

        let outpoints = client
            .blockchain()
            .find_outpoints(boarding_output.address())
            .await?;

        for o in outpoints.iter() {
            // Find outpoints for each boarding output.
            if let ExplorerUtxo {
                outpoint,
                amount,
                confirmation_blocktime: Some(confirmation_blocktime),
                is_spent: false,
            } = o
            {
                let exit_delay_duration: SignedDuration = boarding_output
                    .exit_delay_duration()
                    .try_into()
                    .map_err(Error::ad_hoc)?;
                let spendable_at = Timestamp::new(*confirmation_blocktime as i64, 0)
                    .map_err(Error::ad_hoc)?
                    + exit_delay_duration;

                // For each confirmed outpoint, check if they can already be spent unilaterally
                // using the exit path.
                if spendable_at <= now {
                    tracing::debug!(?outpoint, %amount, ?boarding_output, "Selected boarding output");

                    selected_boarding_outputs.push(unilateral_exit::OnChainInput::new(
                        boarding_output.clone(),
                        *amount,
                        *outpoint,
                    ));
                    selected_amount += *amount;
                }
            }
        }
    }

    let mut selected_vtxo_outputs = Vec::new();

    for (_, vtxo) in client.get_offchain_addresses()? {
        if target_amount <= selected_amount {
            return Ok((selected_boarding_outputs, selected_vtxo_outputs));
        }

        let outpoints = client.blockchain().find_outpoints(vtxo.address()).await?;

        for o in outpoints.iter() {
            // Find outpoints for each VTXO.
            if let ExplorerUtxo {
                outpoint,
                amount,
                confirmation_blocktime: Some(confirmation_blocktime),
                is_spent: false,
            } = o
            {
                // For each confirmed outpoint, check if they can already be spent unilaterally
                // using the exit path.
                if vtxo.can_be_claimed_unilaterally_by_owner(
                    now.as_duration().try_into().map_err(Error::ad_hoc)?,
                    std::time::Duration::from_secs(*confirmation_blocktime),
                ) {
                    tracing::debug!(?outpoint, %amount, ?vtxo, "Selected VTXO");

                    selected_vtxo_outputs.push(unilateral_exit::VtxoInput::new(
                        vtxo.clone(),
                        *amount,
                        *outpoint,
                    ));
                    selected_amount += *amount;
                }
            }
        }
    }

    if selected_amount < target_amount {
        return Err(Error::coin_select(format!(
            "insufficient funds: selected = {selected_amount}, needed = {target_amount}"
        )));
    }

    Ok((selected_boarding_outputs, selected_vtxo_outputs))
}
