# GetVtxoChainRequest

## Properties

| Name           | Type                                                            | Description                                                                                                                                                 | Notes      |
| -------------- | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| **intent**     | Option<[**models::IndexerIntent**](IndexerIntent.md)>           | Intent that directly proves ownership of the vtxo. If passed, the outpoint field is ignored.                                                                | [optional] |
| **outpoint**   | Option<[**models::IndexerOutpoint**](IndexerOutpoint.md)>       |                                                                                                                                                             | [optional] |
| **page**       | Option<[**models::IndexerPageRequest**](IndexerPageRequest.md)> |                                                                                                                                                             | [optional] |
| **page_token** | Option<**String**>                                              | Opaque cursor returned as next_page_token by a previous call. When set, the response resumes from where that page ended.                                    | [optional] |
| **token**      | Option<**String**>                                              | Valid auth_token can also be used if the ownership has already been proved. A valid token obtained from GetVirtualTxs rpc can be recycled for this request. | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
