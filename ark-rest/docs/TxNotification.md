# TxNotification

## Properties

| Name                | Type                                                                       | Description                                                                          | Notes      |
| ------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | ---------- |
| **checkpoint_txs**  | Option<[**std::collections::HashMap<String, models::TxData>**](TxData.md)> | This field is set only in case of offchain tx. key: outpoint, value: checkpoint txid | [optional] |
| **spendable_vtxos** | Option<[**Vec<models::Vtxo>**](Vtxo.md)>                                   |                                                                                      | [optional] |
| **spent_vtxos**     | Option<[**Vec<models::Vtxo>**](Vtxo.md)>                                   |                                                                                      | [optional] |
| **tx**              | Option<**String**>                                                         |                                                                                      | [optional] |
| **txid**            | Option<**String**>                                                         |                                                                                      | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
