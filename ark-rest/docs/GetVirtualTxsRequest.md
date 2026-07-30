# GetVirtualTxsRequest

## Properties

| Name       | Type                                                            | Description                                                                                                                                                | Notes      |
| ---------- | --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| **intent** | Option<[**models::IndexerIntent**](IndexerIntent.md)>           | Intent that directly proves ownership of the transaction inputs. If passed, the txids field is ignored.                                                    | [optional] |
| **page**   | Option<[**models::IndexerPageRequest**](IndexerPageRequest.md)> |                                                                                                                                                            | [optional] |
| **token**  | Option<**String**>                                              | Valid auth_token can also be used if the ownership has already been proved. A valid token obtained from GetVtxoChain rpc can be recycled for this request. | [optional] |
| **txids**  | Option<**Vec<String>**>                                         |                                                                                                                                                            | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
