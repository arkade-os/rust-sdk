# SubscriptionFilter

## Properties

| Name            | Type                                                | Description                                                                                               | Notes      |
| --------------- | --------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ---------- |
| **expressions** | Option<**Vec<String>**>                             | CEL expressions evaluated against each indexed tx envelope. The indexer combines them with OR.            | [optional] |
| **scripts**     | Option<[**models::ScriptFilter**](ScriptFilter.md)> | Script add/remove operations. Will be migrated to a CEL formula in a future protocol version and removed. | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
