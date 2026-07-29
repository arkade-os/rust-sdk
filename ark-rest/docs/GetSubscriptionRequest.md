# GetSubscriptionRequest

## Properties

| Name                | Type                                                            | Description                                                                                                                                                   | Notes      |
| ------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| **filter**          | Option<[**models::SubscriptionFilter**](SubscriptionFilter.md)> | Optional: filter to apply on stream creation. Only used when subscription_id is empty; ignored otherwise. See UpdateSubscriptionRequest for filter semantics. | [optional] |
| **subscription_id** | Option<**String**>                                              | If empty, server creates a new subscription automatically.                                                                                                    | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
