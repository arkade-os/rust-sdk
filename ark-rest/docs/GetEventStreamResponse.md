# GetEventStreamResponse

## Properties

| Name                       | Type                                                                          | Description | Notes      |
| -------------------------- | ----------------------------------------------------------------------------- | ----------- | ---------- |
| **batch_failed**           | Option<[**models::BatchFailedEvent**](BatchFailedEvent.md)>                   |             | [optional] |
| **batch_finalization**     | Option<[**models::BatchFinalizationEvent**](BatchFinalizationEvent.md)>       |             | [optional] |
| **batch_finalized**        | Option<[**models::BatchFinalizedEvent**](BatchFinalizedEvent.md)>             |             | [optional] |
| **batch_started**          | Option<[**models::BatchStartedEvent**](BatchStartedEvent.md)>                 |             | [optional] |
| **heartbeat**              | Option<[**serde_json::Value**](.md)>                                          |             | [optional] |
| **tree_nonces**            | Option<[**models::TreeNoncesEvent**](TreeNoncesEvent.md)>                     |             | [optional] |
| **tree_nonces_aggregated** | Option<[**models::TreeNoncesAggregatedEvent**](TreeNoncesAggregatedEvent.md)> |             | [optional] |
| **tree_signature**         | Option<[**models::TreeSignatureEvent**](TreeSignatureEvent.md)>               |             | [optional] |
| **tree_signing_started**   | Option<[**models::TreeSigningStartedEvent**](TreeSigningStartedEvent.md)>     |             | [optional] |
| **tree_tx**                | Option<[**models::TreeTxEvent**](TreeTxEvent.md)>                             |             | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
