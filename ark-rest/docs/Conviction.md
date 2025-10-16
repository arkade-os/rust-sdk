# Conviction

## Properties

| Name           | Type                                                    | Description                     | Notes      |
| -------------- | ------------------------------------------------------- | ------------------------------- | ---------- |
| **created_at** | Option<**i64**>                                         |                                 | [optional] |
| **crime_type** | Option<[**models::CrimeType**](CrimeType.md)>           |                                 | [optional] |
| **expires_at** | Option<**i64**>                                         | 0 if never expires              | [optional] |
| **id**         | Option<**String**>                                      |                                 | [optional] |
| **pardoned**   | Option<**bool**>                                        |                                 | [optional] |
| **reason**     | Option<**String**>                                      |                                 | [optional] |
| **round_id**   | Option<**String**>                                      |                                 | [optional] |
| **script**     | Option<**String**>                                      | Only set for script convictions | [optional] |
| **r#type**     | Option<[**models::ConvictionType**](ConvictionType.md)> |                                 | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
