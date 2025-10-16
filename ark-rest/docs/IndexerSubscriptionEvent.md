# IndexerSubscriptionEvent

## Properties

| Name               | Type                                                                                     | Description | Notes      |
| ------------------ | ---------------------------------------------------------------------------------------- | ----------- | ---------- |
| **checkpoint_txs** | Option<[**std::collections::HashMap<String, models::IndexerTxData>**](IndexerTxData.md)> |             | [optional] |
| **new_vtxos**      | Option<[**Vec<models::IndexerVtxo>**](IndexerVtxo.md)>                                   |             | [optional] |
| **scripts**        | Option<**Vec<String>**>                                                                  |             | [optional] |
| **spent_vtxos**    | Option<[**Vec<models::IndexerVtxo>**](IndexerVtxo.md)>                                   |             | [optional] |
| **swept_vtxos**    | Option<[**Vec<models::IndexerVtxo>**](IndexerVtxo.md)>                                   |             | [optional] |
| **tx**             | Option<**String**>                                                                       |             | [optional] |
| **txid**           | Option<**String**>                                                                       |             | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
