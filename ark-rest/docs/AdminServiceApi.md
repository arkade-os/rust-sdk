# \AdminServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                                                | HTTP request                                    | Description |
| --------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- | ----------- |
| [**admin_service_ban_script**](AdminServiceApi.md#admin_service_ban_script)                                           | **POST** /v1/admin/conviction/ban               |             |
| [**admin_service_create_note**](AdminServiceApi.md#admin_service_create_note)                                         | **POST** /v1/admin/note                         |             |
| [**admin_service_delete_intents**](AdminServiceApi.md#admin_service_delete_intents)                                   | **POST** /v1/admin/intents/delete               |             |
| [**admin_service_get_active_script_convictions**](AdminServiceApi.md#admin_service_get_active_script_convictions)     | **GET** /v1/admin/convictionsByScript/{script}  |             |
| [**admin_service_get_convictions**](AdminServiceApi.md#admin_service_get_convictions)                                 | **GET** /v1/admin/convictions/{ids}             |             |
| [**admin_service_get_convictions_by_round**](AdminServiceApi.md#admin_service_get_convictions_by_round)               | **GET** /v1/admin/convictionsByRound/{round_id} |             |
| [**admin_service_get_convictions_in_range**](AdminServiceApi.md#admin_service_get_convictions_in_range)               | **GET** /v1/admin/convictionsInRange            |             |
| [**admin_service_get_round_details**](AdminServiceApi.md#admin_service_get_round_details)                             | **GET** /v1/admin/round/{round_id}              |             |
| [**admin_service_get_rounds**](AdminServiceApi.md#admin_service_get_rounds)                                           | **GET** /v1/admin/rounds                        |             |
| [**admin_service_get_scheduled_session_config**](AdminServiceApi.md#admin_service_get_scheduled_session_config)       | **GET** /v1/admin/scheduledSession              |             |
| [**admin_service_get_scheduled_sweep**](AdminServiceApi.md#admin_service_get_scheduled_sweep)                         | **GET** /v1/admin/sweeps                        |             |
| [**admin_service_list_intents**](AdminServiceApi.md#admin_service_list_intents)                                       | **GET** /v1/admin/intents                       |             |
| [**admin_service_pardon_conviction**](AdminServiceApi.md#admin_service_pardon_conviction)                             | **POST** /v1/admin/convictions/{id}/pardon      |             |
| [**admin_service_update_scheduled_session_config**](AdminServiceApi.md#admin_service_update_scheduled_session_config) | **POST** /v1/admin/scheduledSession             |             |

## admin_service_ban_script

> serde_json::Value admin_service_ban_script(ban_script_request)

### Parameters

| Name                   | Type                                        | Description | Required   | Notes |
| ---------------------- | ------------------------------------------- | ----------- | ---------- | ----- |
| **ban_script_request** | [**BanScriptRequest**](BanScriptRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_create_note

> models::CreateNoteResponse admin_service_create_note(create_note_request)

### Parameters

| Name                    | Type                                          | Description | Required   | Notes |
| ----------------------- | --------------------------------------------- | ----------- | ---------- | ----- |
| **create_note_request** | [**CreateNoteRequest**](CreateNoteRequest.md) |             | [required] |       |

### Return type

[**models::CreateNoteResponse**](CreateNoteResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_delete_intents

> serde_json::Value admin_service_delete_intents(delete_intents_request)

### Parameters

| Name                       | Type                                                | Description | Required   | Notes |
| -------------------------- | --------------------------------------------------- | ----------- | ---------- | ----- |
| **delete_intents_request** | [**DeleteIntentsRequest**](DeleteIntentsRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_active_script_convictions

> models::GetActiveScriptConvictionsResponse admin_service_get_active_script_convictions(script)

### Parameters

| Name       | Type       | Description | Required   | Notes |
| ---------- | ---------- | ----------- | ---------- | ----- |
| **script** | **String** |             | [required] |       |

### Return type

[**models::GetActiveScriptConvictionsResponse**](GetActiveScriptConvictionsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_convictions

> models::GetConvictionsResponse admin_service_get_convictions(ids)

### Parameters

| Name    | Type                         | Description | Required   | Notes |
| ------- | ---------------------------- | ----------- | ---------- | ----- |
| **ids** | [**Vec<String>**](String.md) |             | [required] |       |

### Return type

[**models::GetConvictionsResponse**](GetConvictionsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_convictions_by_round

> models::GetConvictionsByRoundResponse admin_service_get_convictions_by_round(round_id)

### Parameters

| Name         | Type       | Description | Required   | Notes |
| ------------ | ---------- | ----------- | ---------- | ----- |
| **round_id** | **String** |             | [required] |       |

### Return type

[**models::GetConvictionsByRoundResponse**](GetConvictionsByRoundResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_convictions_in_range

> models::GetConvictionsInRangeResponse admin_service_get_convictions_in_range(from, to)

### Parameters

| Name     | Type            | Description    | Required | Notes |
| -------- | --------------- | -------------- | -------- | ----- |
| **from** | Option<**i64**> | Unix timestamp |          |       |
| **to**   | Option<**i64**> | Unix timestamp |          |       |

### Return type

[**models::GetConvictionsInRangeResponse**](GetConvictionsInRangeResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_round_details

> models::GetRoundDetailsResponse admin_service_get_round_details(round_id)

### Parameters

| Name         | Type       | Description | Required   | Notes |
| ------------ | ---------- | ----------- | ---------- | ----- |
| **round_id** | **String** |             | [required] |       |

### Return type

[**models::GetRoundDetailsResponse**](GetRoundDetailsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_rounds

> models::GetRoundsResponse admin_service_get_rounds(after, before)

### Parameters

| Name       | Type            | Description | Required | Notes |
| ---------- | --------------- | ----------- | -------- | ----- |
| **after**  | Option<**i64**> |             |          |       |
| **before** | Option<**i64**> |             |          |       |

### Return type

[**models::GetRoundsResponse**](GetRoundsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_scheduled_session_config

> models::GetScheduledSessionConfigResponse admin_service_get_scheduled_session_config()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::GetScheduledSessionConfigResponse**](GetScheduledSessionConfigResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_get_scheduled_sweep

> models::GetScheduledSweepResponse admin_service_get_scheduled_sweep()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::GetScheduledSweepResponse**](GetScheduledSweepResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_list_intents

> models::ListIntentsResponse admin_service_list_intents(intent_ids)

### Parameters

| Name           | Type                                 | Description | Required | Notes |
| -------------- | ------------------------------------ | ----------- | -------- | ----- |
| **intent_ids** | Option<[**Vec<String>**](String.md)> |             |          |       |

### Return type

[**models::ListIntentsResponse**](ListIntentsResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_pardon_conviction

> serde_json::Value admin_service_pardon_conviction(id, body)

### Parameters

| Name     | Type                  | Description | Required   | Notes |
| -------- | --------------------- | ----------- | ---------- | ----- |
| **id**   | **String**            |             | [required] |       |
| **body** | **serde_json::Value** |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## admin_service_update_scheduled_session_config

> serde_json::Value admin_service_update_scheduled_session_config(update_scheduled_session_config_request)

### Parameters

| Name                                        | Type                                                                              | Description | Required   | Notes |
| ------------------------------------------- | --------------------------------------------------------------------------------- | ----------- | ---------- | ----- |
| **update_scheduled_session_config_request** | [**UpdateScheduledSessionConfigRequest**](UpdateScheduledSessionConfigRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
