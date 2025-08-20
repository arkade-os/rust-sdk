# V1GetEventStreamResponse

## Properties

| Name                       | Type                                                                              | Description | Notes      |
| -------------------------- | --------------------------------------------------------------------------------- | ----------- | ---------- |
| **batch_started**          | Option<[**models::V1BatchStartedEvent**](v1BatchStartedEvent.md)>                 |             | [optional] |
| **batch_finalization**     | Option<[**models::V1BatchFinalizationEvent**](v1BatchFinalizationEvent.md)>       |             | [optional] |
| **batch_finalized**        | Option<[**models::V1BatchFinalizedEvent**](v1BatchFinalizedEvent.md)>             |             | [optional] |
| **batch_failed**           | Option<[**models::V1BatchFailedEvent**](v1BatchFailedEvent.md)>                   |             | [optional] |
| **tree_signing_started**   | Option<[**models::V1TreeSigningStartedEvent**](v1TreeSigningStartedEvent.md)>     |             | [optional] |
| **tree_nonces_aggregated** | Option<[**models::V1TreeNoncesAggregatedEvent**](v1TreeNoncesAggregatedEvent.md)> |             | [optional] |
| **tree_tx**                | Option<[**models::V1TreeTxEvent**](v1TreeTxEvent.md)>                             |             | [optional] |
| **tree_signature**         | Option<[**models::V1TreeSignatureEvent**](v1TreeSignatureEvent.md)>               |             | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
