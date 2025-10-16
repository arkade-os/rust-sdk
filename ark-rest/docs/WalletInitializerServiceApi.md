# \WalletInitializerServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                                            | HTTP request                      | Description |
| ----------------------------------------------------------------------------------------------------------------- | --------------------------------- | ----------- |
| [**wallet_initializer_service_create**](WalletInitializerServiceApi.md#wallet_initializer_service_create)         | **POST** /v1/admin/wallet/create  |             |
| [**wallet_initializer_service_gen_seed**](WalletInitializerServiceApi.md#wallet_initializer_service_gen_seed)     | **GET** /v1/admin/wallet/seed     |             |
| [**wallet_initializer_service_get_status**](WalletInitializerServiceApi.md#wallet_initializer_service_get_status) | **GET** /v1/admin/wallet/status   |             |
| [**wallet_initializer_service_restore**](WalletInitializerServiceApi.md#wallet_initializer_service_restore)       | **POST** /v1/admin/wallet/restore |             |
| [**wallet_initializer_service_unlock**](WalletInitializerServiceApi.md#wallet_initializer_service_unlock)         | **POST** /v1/admin/wallet/unlock  |             |

## wallet_initializer_service_create

> serde_json::Value wallet_initializer_service_create(create_request)

### Parameters

| Name               | Type                                  | Description | Required   | Notes |
| ------------------ | ------------------------------------- | ----------- | ---------- | ----- |
| **create_request** | [**CreateRequest**](CreateRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_initializer_service_gen_seed

> models::GenSeedResponse wallet_initializer_service_gen_seed()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::GenSeedResponse**](GenSeedResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_initializer_service_get_status

> models::GetStatusResponse wallet_initializer_service_get_status()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::GetStatusResponse**](GetStatusResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_initializer_service_restore

> serde_json::Value wallet_initializer_service_restore(restore_request)

### Parameters

| Name                | Type                                    | Description | Required   | Notes |
| ------------------- | --------------------------------------- | ----------- | ---------- | ----- |
| **restore_request** | [**RestoreRequest**](RestoreRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_initializer_service_unlock

> serde_json::Value wallet_initializer_service_unlock(unlock_request)

### Parameters

| Name               | Type                                  | Description | Required   | Notes |
| ------------------ | ------------------------------------- | ----------- | ---------- | ----- |
| **unlock_request** | [**UnlockRequest**](UnlockRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
