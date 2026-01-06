# \IndexerServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                                                | HTTP request                                                                      | Description |
| --------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | ----------- |
| [**indexer_service_get_batch_sweep_transactions**](IndexerServiceApi.md#indexer_service_get_batch_sweep_transactions) | **GET** /v1/indexer/batch/{batch_outpoint.txid}/{batch_outpoint.vout}/sweepTxs    |             |
| [**indexer_service_get_commitment_tx**](IndexerServiceApi.md#indexer_service_get_commitment_tx)                       | **GET** /v1/indexer/commitmentTx/{txid}                                           |             |
| [**indexer_service_get_connectors**](IndexerServiceApi.md#indexer_service_get_connectors)                             | **GET** /v1/indexer/commitmentTx/{txid}/connectors                                |             |
| [**indexer_service_get_forfeit_txs**](IndexerServiceApi.md#indexer_service_get_forfeit_txs)                           | **GET** /v1/indexer/commitmentTx/{txid}/forfeitTxs                                |             |
| [**indexer_service_get_subscription**](IndexerServiceApi.md#indexer_service_get_subscription)                         | **GET** /v1/indexer/script/subscription/{subscription_id}                         |             |
| [**indexer_service_get_virtual_txs**](IndexerServiceApi.md#indexer_service_get_virtual_txs)                           | **GET** /v1/indexer/virtualTx/{txids}                                             |             |
| [**indexer_service_get_vtxo_chain**](IndexerServiceApi.md#indexer_service_get_vtxo_chain)                             | **GET** /v1/indexer/vtxo/{outpoint.txid}/{outpoint.vout}/chain                    |             |
| [**indexer_service_get_vtxo_tree**](IndexerServiceApi.md#indexer_service_get_vtxo_tree)                               | **GET** /v1/indexer/batch/{batch_outpoint.txid}/{batch_outpoint.vout}/tree        |             |
| [**indexer_service_get_vtxo_tree_leaves**](IndexerServiceApi.md#indexer_service_get_vtxo_tree_leaves)                 | **GET** /v1/indexer/batch/{batch_outpoint.txid}/{batch_outpoint.vout}/tree/leaves |             |
| [**indexer_service_get_vtxos**](IndexerServiceApi.md#indexer_service_get_vtxos)                                       | **GET** /v1/indexer/vtxos                                                         |             |
| [**indexer_service_subscribe_for_scripts**](IndexerServiceApi.md#indexer_service_subscribe_for_scripts)               | **POST** /v1/indexer/script/subscribe                                             |             |
| [**indexer_service_unsubscribe_for_scripts**](IndexerServiceApi.md#indexer_service_unsubscribe_for_scripts)           | **POST** /v1/indexer/script/unsubscribe                                           |             |

## indexer_service_get_batch_sweep_transactions

> models::GetBatchSweepTransactionsResponse indexer_service_get_batch_sweep_transactions(batch_outpoint_period_txid, batch_outpoint_period_vout)

GetBatchSweepTransactions returns the list of transaction (txid) that swept a given batch output. In most cases the list contains only one txid, meaning that all the amount locked for a vtxo tree has been claimed back. If any of the leaves of the tree have been unrolled onchain before the expiration, the list will contain many txids instead. In a binary tree with 4 or more leaves, 1 unroll causes the server to broadcast 3 txs to sweep the whole rest of tree for example. If a whole vtxo tree has been unrolled onchain, the list of txids for that batch output is empty.

### Parameters

| Name                           | Type       | Description | Required   | Notes |
| ------------------------------ | ---------- | ----------- | ---------- | ----- |
| **batch_outpoint_period_txid** | **String** |             | [required] |       |
| **batch_outpoint_period_vout** | **i32**    |             | [required] |       |

### Return type

[**models::GetBatchSweepTransactionsResponse**](GetBatchSweepTransactionsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_commitment_tx

> models::GetCommitmentTxResponse indexer_service_get_commitment_tx(txid)

GetCommitmentTx returns information about a specific commitment transaction identified by the provided txid.

### Parameters

| Name     | Type       | Description | Required   | Notes |
| -------- | ---------- | ----------- | ---------- | ----- |
| **txid** | **String** |             | [required] |       |

### Return type

[**models::GetCommitmentTxResponse**](GetCommitmentTxResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_connectors

> models::GetConnectorsResponse indexer_service_get_connectors(txid, page_period_size, page_period_index)

GetConnectors returns the tree of connectors for the provided commitment transaction. The response includes a list of connector txs with details on the tree posistion and may include pagination information if the results span multiple pages.

### Parameters

| Name                  | Type            | Description | Required   | Notes |
| --------------------- | --------------- | ----------- | ---------- | ----- |
| **txid**              | **String**      |             | [required] |       |
| **page_period_size**  | Option<**i32**> |             |            |       |
| **page_period_index** | Option<**i32**> |             |            |       |

### Return type

[**models::GetConnectorsResponse**](GetConnectorsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_forfeit_txs

> models::GetForfeitTxsResponse indexer_service_get_forfeit_txs(txid, page_period_size, page_period_index)

GetForfeitTxs returns the list of forfeit transactions that were submitted for the provided commitment transaction. The response may include pagination information if the results span multiple pages.

### Parameters

| Name                  | Type            | Description | Required   | Notes |
| --------------------- | --------------- | ----------- | ---------- | ----- |
| **txid**              | **String**      |             | [required] |       |
| **page_period_size**  | Option<**i32**> |             |            |       |
| **page_period_index** | Option<**i32**> |             |            |       |

### Return type

[**models::GetForfeitTxsResponse**](GetForfeitTxsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_subscription

> models::GetSubscriptionResponse indexer_service_get_subscription(subscription_id)

GetSubscription is a server-side streaming RPC which allows clients to receive real-time notifications on transactions related to the subscribed vtxo scripts. The subscription can be created or updated by using the SubscribeForScripts and UnsubscribeForScripts RPCs.

### Parameters

| Name                | Type       | Description | Required   | Notes |
| ------------------- | ---------- | ----------- | ---------- | ----- |
| **subscription_id** | **String** |             | [required] |       |

### Return type

[**models::GetSubscriptionResponse**](GetSubscriptionResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: text/event-stream, application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_virtual_txs

> models::GetVirtualTxsResponse indexer_service_get_virtual_txs(txids, page_period_size, page_period_index)

GetVirtualTxs returns the virtual transactions in hex format for the specified txids. The response may be paginated if the results span multiple pages.

### Parameters

| Name                  | Type                         | Description | Required   | Notes |
| --------------------- | ---------------------------- | ----------- | ---------- | ----- |
| **txids**             | [**Vec<String>**](String.md) |             | [required] |       |
| **page_period_size**  | Option<**i32**>              |             |            |       |
| **page_period_index** | Option<**i32**>              |             |            |       |

### Return type

[**models::GetVirtualTxsResponse**](GetVirtualTxsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_vtxo_chain

> models::GetVtxoChainResponse indexer_service_get_vtxo_chain(outpoint_period_txid, outpoint_period_vout, page_period_size, page_period_index)

GetVtxoChain returns the the chain of ark txs that starts from spending any vtxo leaf and ends with the creation of the provided vtxo outpoint. The response may be paginated if the results span multiple pages.

### Parameters

| Name                     | Type            | Description | Required   | Notes |
| ------------------------ | --------------- | ----------- | ---------- | ----- |
| **outpoint_period_txid** | **String**      |             | [required] |       |
| **outpoint_period_vout** | **i32**         |             | [required] |       |
| **page_period_size**     | Option<**i32**> |             |            |       |
| **page_period_index**    | Option<**i32**> |             |            |       |

### Return type

[**models::GetVtxoChainResponse**](GetVtxoChainResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_vtxo_tree

> models::GetVtxoTreeResponse indexer_service_get_vtxo_tree(batch_outpoint_period_txid, batch_outpoint_period_vout, page_period_size, page_period_index)

GetVtxoTree returns the vtxo tree for the provided batch outpoint. The response includes a list of txs with details on the tree posistion and may include pagination information if the results span multiple pages.

### Parameters

| Name                           | Type            | Description | Required   | Notes |
| ------------------------------ | --------------- | ----------- | ---------- | ----- |
| **batch_outpoint_period_txid** | **String**      |             | [required] |       |
| **batch_outpoint_period_vout** | **i32**         |             | [required] |       |
| **page_period_size**           | Option<**i32**> |             |            |       |
| **page_period_index**          | Option<**i32**> |             |            |       |

### Return type

[**models::GetVtxoTreeResponse**](GetVtxoTreeResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_vtxo_tree_leaves

> models::GetVtxoTreeLeavesResponse indexer_service_get_vtxo_tree_leaves(batch_outpoint_period_txid, batch_outpoint_period_vout, page_period_size, page_period_index)

GetVtxoTreeLeaves returns the list of leaves (vtxo outpoints) of the tree(s) for the provided batch outpoint. The response may be paginated if the results span multiple pages.

### Parameters

| Name                           | Type            | Description | Required   | Notes |
| ------------------------------ | --------------- | ----------- | ---------- | ----- |
| **batch_outpoint_period_txid** | **String**      |             | [required] |       |
| **batch_outpoint_period_vout** | **i32**         |             | [required] |       |
| **page_period_size**           | Option<**i32**> |             |            |       |
| **page_period_index**          | Option<**i32**> |             |            |       |

### Return type

[**models::GetVtxoTreeLeavesResponse**](GetVtxoTreeLeavesResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_get_vtxos

> models::GetVtxosResponse indexer_service_get_vtxos(scripts, outpoints, spendable_only, spent_only, recoverable_only, pending_only, page_period_size, page_period_index)

GetVtxos returns the list of vtxos based on the provided filter. Vtxos can be retrieved either by addresses or by outpoints, and optionally filtered by spendable or spent only. The response may be paginated if the results span multiple pages.

### Parameters

| Name                  | Type                                 | Description                                                                                            | Required | Notes |
| --------------------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------ | -------- | ----- |
| **scripts**           | Option<[**Vec<String>**](String.md)> | Either specify a list of vtxo scripts.                                                                 |          |       |
| **outpoints**         | Option<[**Vec<String>**](String.md)> | Or specify a list of vtxo outpoints. The 2 filters are mutually exclusive.                             |          |       |
| **spendable_only**    | Option<**bool**>                     | Retrieve only spendable vtxos                                                                          |          |       |
| **spent_only**        | Option<**bool**>                     | Retrieve only spent vtxos.                                                                             |          |       |
| **recoverable_only**  | Option<**bool**>                     | Retrieve only recoverable vtxos (notes, subdust or swept vtxos). The 3 filters are mutually exclusive, |          |       |
| **pending_only**      | Option<**bool**>                     | Include only spent vtxos that are not finalized.                                                       |          |       |
| **page_period_size**  | Option<**i32**>                      |                                                                                                        |          |       |
| **page_period_index** | Option<**i32**>                      |                                                                                                        |          |       |

### Return type

[**models::GetVtxosResponse**](GetVtxosResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_subscribe_for_scripts

> models::SubscribeForScriptsResponse indexer_service_subscribe_for_scripts(subscribe_for_scripts_request)

SubscribeForScripts allows to subscribe for tx notifications related to the provided vtxo scripts. It can also be used to update an existing subscribtion by adding new scripts to it.

### Parameters

| Name                              | Type                                                            | Description | Required   | Notes |
| --------------------------------- | --------------------------------------------------------------- | ----------- | ---------- | ----- |
| **subscribe_for_scripts_request** | [**SubscribeForScriptsRequest**](SubscribeForScriptsRequest.md) |             | [required] |       |

### Return type

[**models::SubscribeForScriptsResponse**](SubscribeForScriptsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## indexer_service_unsubscribe_for_scripts

> serde_json::Value indexer_service_unsubscribe_for_scripts(unsubscribe_for_scripts_request)

UnsubscribeForScripts allows to remove scripts from an existing subscription.

### Parameters

| Name                                | Type                                                                | Description | Required   | Notes |
| ----------------------------------- | ------------------------------------------------------------------- | ----------- | ---------- | ----- |
| **unsubscribe_for_scripts_request** | [**UnsubscribeForScriptsRequest**](UnsubscribeForScriptsRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
