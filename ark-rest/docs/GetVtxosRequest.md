# GetVtxosRequest

## Properties

| Name                 | Type                                                            | Description                                                                                            | Notes      |
| -------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ | ---------- |
| **outpoints**        | Option<**Vec<String>**>                                         | Or specify a list of vtxo outpoints. The 2 filters are mutually exclusive.                             | [optional] |
| **page**             | Option<[**models::IndexerPageRequest**](IndexerPageRequest.md)> |                                                                                                        | [optional] |
| **pending_only**     | Option<**bool**>                                                | Include only spent vtxos that are not finalized.                                                       | [optional] |
| **recoverable_only** | Option<**bool**>                                                | Retrieve only recoverable vtxos (notes, subdust or swept vtxos). The 3 filters are mutually exclusive, | [optional] |
| **scripts**          | Option<**Vec<String>**>                                         | Either specify a list of vtxo scripts.                                                                 | [optional] |
| **spendable_only**   | Option<**bool**>                                                | Retrieve only spendable vtxos                                                                          | [optional] |
| **spent_only**       | Option<**bool**>                                                | Retrieve only spent vtxos.                                                                             | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
