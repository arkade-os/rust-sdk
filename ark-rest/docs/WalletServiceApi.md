# \WalletServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                 | HTTP request                       | Description |
| -------------------------------------------------------------------------------------- | ---------------------------------- | ----------- |
| [**wallet_service_derive_address**](WalletServiceApi.md#wallet_service_derive_address) | **GET** /v1/admin/wallet/address   |             |
| [**wallet_service_get_balance**](WalletServiceApi.md#wallet_service_get_balance)       | **GET** /v1/admin/wallet/balance   |             |
| [**wallet_service_lock**](WalletServiceApi.md#wallet_service_lock)                     | **POST** /v1/admin/wallet/lock     |             |
| [**wallet_service_withdraw**](WalletServiceApi.md#wallet_service_withdraw)             | **POST** /v1/admin/wallet/withdraw |             |

## wallet_service_derive_address

> models::DeriveAddressResponse wallet_service_derive_address()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::DeriveAddressResponse**](DeriveAddressResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_service_get_balance

> models::GetBalanceResponse wallet_service_get_balance()

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::GetBalanceResponse**](GetBalanceResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_service_lock

> serde_json::Value wallet_service_lock(body)

### Parameters

| Name     | Type                  | Description | Required   | Notes |
| -------- | --------------------- | ----------- | ---------- | ----- |
| **body** | **serde_json::Value** |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

## wallet_service_withdraw

> models::WithdrawResponse wallet_service_withdraw(withdraw_request)

### Parameters

| Name                 | Type                                      | Description | Required   | Notes |
| -------------------- | ----------------------------------------- | ----------- | ---------- | ----- |
| **withdraw_request** | [**WithdrawRequest**](WithdrawRequest.md) |             | [required] |       |

### Return type

[**models::WithdrawResponse**](WithdrawResponse.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
