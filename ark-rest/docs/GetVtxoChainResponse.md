# GetVtxoChainResponse

## Properties

| Name                | Type                                                              | Description                                                                                    | Notes      |
| ------------------- | ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ---------- |
| **auth_token**      | Option<**String**>                                                | Auth token can be used for other rpcs related to this vtxo/tx that require proof of ownership. | [optional] |
| **chain**           | Option<[**Vec<models::IndexerChain>**](IndexerChain.md)>          |                                                                                                | [optional] |
| **next_page_token** | Option<**String**>                                                | Opaque cursor for fetching the next page. Empty when there are no more pages.                  | [optional] |
| **page**            | Option<[**models::IndexerPageResponse**](IndexerPageResponse.md)> |                                                                                                | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
