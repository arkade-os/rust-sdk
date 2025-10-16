# \SignerManagerServiceApi

All URIs are relative to _http://localhost_

| Method                                                                                                  | HTTP request              | Description |
| ------------------------------------------------------------------------------------------------------- | ------------------------- | ----------- |
| [**signer_manager_service_load_signer**](SignerManagerServiceApi.md#signer_manager_service_load_signer) | **POST** /v1/admin/signer |             |

## signer_manager_service_load_signer

> serde_json::Value signer_manager_service_load_signer(load_signer_request)

### Parameters

| Name                    | Type                                          | Description | Required   | Notes |
| ----------------------- | --------------------------------------------- | ----------- | ---------- | ----- |
| **load_signer_request** | [**LoadSignerRequest**](LoadSignerRequest.md) |             | [required] |       |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

No authorization required

### HTTP request headers

- **Content-Type**: application/json
- **Accept**: application/json

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)
