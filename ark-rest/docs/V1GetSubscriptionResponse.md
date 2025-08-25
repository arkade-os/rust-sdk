# V1GetSubscriptionResponse

## Properties

| Name               | Type                                                                                         | Description | Notes      |
| ------------------ | -------------------------------------------------------------------------------------------- | ----------- | ---------- |
| **txid**           | Option<**String**>                                                                           |             | [optional] |
| **scripts**        | Option<**Vec<String>**>                                                                      |             | [optional] |
| **new_vtxos**      | Option<[**Vec<models::V1IndexerVtxo>**](v1IndexerVtxo.md)>                                   |             | [optional] |
| **spent_vtxos**    | Option<[**Vec<models::V1IndexerVtxo>**](v1IndexerVtxo.md)>                                   |             | [optional] |
| **tx**             | Option<**String**>                                                                           |             | [optional] |
| **checkpoint_txs** | Option<[**std::collections::HashMap<String, models::V1IndexerTxData>**](v1IndexerTxData.md)> |             | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
