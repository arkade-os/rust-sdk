# V1TxNotification

## Properties

| Name                | Type                                                                           | Description                                                                          | Notes      |
| ------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------ | ---------- |
| **txid**            | Option<**String**>                                                             |                                                                                      | [optional] |
| **tx**              | Option<**String**>                                                             |                                                                                      | [optional] |
| **spent_vtxos**     | Option<[**Vec<models::V1Vtxo>**](v1Vtxo.md)>                                   |                                                                                      | [optional] |
| **spendable_vtxos** | Option<[**Vec<models::V1Vtxo>**](v1Vtxo.md)>                                   |                                                                                      | [optional] |
| **checkpoint_txs**  | Option<[**std::collections::HashMap<String, models::V1TxData>**](v1TxData.md)> | This field is set only in case of offchain tx. key: outpoint, value: checkpoint txid | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
