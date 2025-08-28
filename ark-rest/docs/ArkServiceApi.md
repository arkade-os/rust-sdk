# \ArkServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                              | HTTP request                             | Description                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| --------------------------------------------------------------------------------------------------- | ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [**ark_service_confirm_registration**](ArkServiceApi.md#ark_service_confirm_registration)           | **POST** /v1/batch/ack                   | ConfirmRegistration allows a client that has been selected for the next batch to confirm its participation by revealing the intent id.                                                                                                                                                                                                                                                                                                           |
| [**ark_service_delete_intent**](ArkServiceApi.md#ark_service_delete_intent)                         | **POST** /v1/batch/deleteIntent          | DeleteIntent removes a previously registered intent from the server. The client should provide the BIP-322 signature and message including any of the vtxos used in the registered intent to prove its ownership. The server should delete the intent and return success.                                                                                                                                                                        |
| [**ark_service_finalize_tx**](ArkServiceApi.md#ark_service_finalize_tx)                             | **POST** /v1/tx/finalize                 | FinalizeTx is the last lef of the process of spending vtxos offchain and allows a client to submit the fully signed checkpoint txs for the provided Ark txid . The server verifies the signed checkpoint transactions and returns success if everything is valid.                                                                                                                                                                                |
| [**ark_service_get_event_stream**](ArkServiceApi.md#ark_service_get_event_stream)                   | **GET** /v1/batch/events                 | GetEventStream is a server-side streaming RPC that allows clients to receive a stream of events related to batch processing. Clients should use this stream as soon as they are ready to join a batch and can listen for various events such as batch start, batch finalization, and other related activities. The server pushes these events to the client in real-time as soon as its ready to move to the next phase of the batch processing. |
| [**ark_service_get_info**](ArkServiceApi.md#ark_service_get_info)                                   | **GET** /v1/info                         | GetInfo returns information and parameters of the server.                                                                                                                                                                                                                                                                                                                                                                                        |
| [**ark_service_get_transactions_stream**](ArkServiceApi.md#ark_service_get_transactions_stream)     | **GET** /v1/txs                          | GetTransactionsStream is a server-side streaming RPC that allows clients to receive notifications in real-time about any commitment tx or ark tx processed and finalized by the server. NOTE: the stream doesn't have history support, therefore returns only txs from the moment it's opened until it's closed.                                                                                                                                 |
| [**ark_service_register_intent**](ArkServiceApi.md#ark_service_register_intent)                     | **POST** /v1/batch/registerIntent        | RegisterIntent allows to register a new intent that will be eventually selected by the server for a particular batch. The client should provide a BIP-322 message with the intent information, and the server should respond with an intent id.                                                                                                                                                                                                  |
| [**ark_service_submit_signed_forfeit_txs**](ArkServiceApi.md#ark_service_submit_signed_forfeit_txs) | **POST** /v1/batch/submitForfeitTxs      | SubmitSignedForfeitTxs allows a client to submit signed forfeit transactions and/or signed commitment transaction (in case of onboarding). The server should verify the signed txs and return success.                                                                                                                                                                                                                                           |
| [**ark_service_submit_tree_nonces**](ArkServiceApi.md#ark_service_submit_tree_nonces)               | **POST** /v1/batch/tree/submitNonces     | SubmitTreeNonces allows a cosigner to submit the tree nonces for the musig2 session of a given batch. The client should provide the batch id, the cosigner public key, and the tree nonces. The server should verify the cosigner public key and the nonces, and store them for later aggregation once nonces from all clients are collected.                                                                                                    |
| [**ark_service_submit_tree_signatures**](ArkServiceApi.md#ark_service_submit_tree_signatures)       | **POST** /v1/batch/tree/submitSignatures | SubmitTreeSignatures allows a cosigner to submit the tree signatures for the musig2 session of a given batch. The client should provide the batch id, the cosigner public key, and the tree signatures. The server should verify the cosigner public key and the signatures, and store them for later aggregation once signatures from all clients are collected.                                                                                |
| [**ark_service_submit_tx**](ArkServiceApi.md#ark_service_submit_tx)                                 | **POST** /v1/tx/submit                   | SubmitTx is the first leg of the process of spending vtxos offchain and allows a client to submit a signed Ark transaction and the unsigned checkpoint transactions. The server should verify the signed transactions and return the fully signed Ark tx and the signed checkpoint txs.                                                                                                                                                          |

## ark_service_confirm_registration

> serde_json::Value ark_service_confirm_registration(body)
> ConfirmRegistration allows a client that has been selected for the next batch to confirm its participation by revealing the intent id.

### Parameters

| Name     | Type                                                                | Description | Required   | Notes |
| -------- | ------------------------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1ConfirmRegistrationRequest**](V1ConfirmRegistrationRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_delete_intent

> serde_json::Value ark_service_delete_intent(body)
> DeleteIntent removes a previously registered intent from the server. The client should provide the BIP-322 signature and message including any of the vtxos used in the registered intent to prove its ownership. The server should delete the intent and return success.

### Parameters

| Name     | Type                                                  | Description | Required   | Notes |
| -------- | ----------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1DeleteIntentRequest**](V1DeleteIntentRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_finalize_tx

> serde_json::Value ark_service_finalize_tx(body)
> FinalizeTx is the last lef of the process of spending vtxos offchain and allows a client to submit the fully signed checkpoint txs for the provided Ark txid . The server verifies the signed checkpoint transactions and returns success if everything is valid.

### Parameters

| Name     | Type                                              | Description | Required   | Notes |
| -------- | ------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1FinalizeTxRequest**](V1FinalizeTxRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_get_event_stream

> models::StreamResultOfV1GetEventStreamResponse ark_service_get_event_stream(topics)
> GetEventStream is a server-side streaming RPC that allows clients to receive a stream of events related to batch processing. Clients should use this stream as soon as they are ready to join a batch and can listen for various events such as batch start, batch finalization, and other related activities. The server pushes these events to the client in real-time as soon as its ready to move to the next phase of the batch processing.

### Parameters

| Name       | Type                                 | Description | Required | Notes |
| ---------- | ------------------------------------ | ----------- | -------- | ----- |
| **topics** | Option<[**Vec<String>**](String.md)> |             |          |       |

### Return type

[**models::StreamResultOfV1GetEventStreamResponse**](Stream_result_of_v1GetEventStreamResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_get_info

> models::V1GetInfoResponse ark_service_get_info()
> GetInfo returns information and parameters of the server.

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::V1GetInfoResponse**](v1GetInfoResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_get_transactions_stream

> models::StreamResultOfV1GetTransactionsStreamResponse ark_service_get_transactions_stream()
> GetTransactionsStream is a server-side streaming RPC that allows clients to receive notifications in real-time about any commitment tx or ark tx processed and finalized by the server. NOTE: the stream doesn't have history support, therefore returns only txs from the moment it's opened until it's closed.

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::StreamResultOfV1GetTransactionsStreamResponse**](Stream_result_of_v1GetTransactionsStreamResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_register_intent

> models::V1RegisterIntentResponse ark_service_register_intent(body)
> RegisterIntent allows to register a new intent that will be eventually selected by the server for a particular batch. The client should provide a BIP-322 message with the intent information, and the server should respond with an intent id.

### Parameters

| Name     | Type                                                      | Description | Required   | Notes |
| -------- | --------------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1RegisterIntentRequest**](V1RegisterIntentRequest.md) |             | [required] |       |

### Return type

[**models::V1RegisterIntentResponse**](v1RegisterIntentResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_submit_signed_forfeit_txs

> serde_json::Value ark_service_submit_signed_forfeit_txs(body)
> SubmitSignedForfeitTxs allows a client to submit signed forfeit transactions and/or signed commitment transaction (in case of onboarding). The server should verify the signed txs and return success.

### Parameters

| Name     | Type                                                                      | Description | Required   | Notes |
| -------- | ------------------------------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1SubmitSignedForfeitTxsRequest**](V1SubmitSignedForfeitTxsRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_submit_tree_nonces

> serde_json::Value ark_service_submit_tree_nonces(body)
> SubmitTreeNonces allows a cosigner to submit the tree nonces for the musig2 session of a given batch. The client should provide the batch id, the cosigner public key, and the tree nonces. The server should verify the cosigner public key and the nonces, and store them for later aggregation once nonces from all clients are collected.

### Parameters

| Name     | Type                                                          | Description | Required   | Notes |
| -------- | ------------------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1SubmitTreeNoncesRequest**](V1SubmitTreeNoncesRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_submit_tree_signatures

> serde_json::Value ark_service_submit_tree_signatures(body)
> SubmitTreeSignatures allows a cosigner to submit the tree signatures for the musig2 session of a given batch. The client should provide the batch id, the cosigner public key, and the tree signatures. The server should verify the cosigner public key and the signatures, and store them for later aggregation once signatures from all clients are collected.

### Parameters

| Name     | Type                                                                  | Description | Required   | Notes |
| -------- | --------------------------------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1SubmitTreeSignaturesRequest**](V1SubmitTreeSignaturesRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## ark_service_submit_tx

> models::V1SubmitTxResponse ark_service_submit_tx(body)
> SubmitTx is the first leg of the process of spending vtxos offchain and allows a client to submit a signed Ark transaction and the unsigned checkpoint transactions. The server should verify the signed transactions and return the fully signed Ark tx and the signed checkpoint txs.

### Parameters

| Name     | Type                                          | Description | Required   | Notes |
| -------- | --------------------------------------------- | ----------- | ---------- | ----- |
| **body** | [**V1SubmitTxRequest**](V1SubmitTxRequest.md) |             | [required] |       |

### Return type

[**models::V1SubmitTxResponse**](v1SubmitTxResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
