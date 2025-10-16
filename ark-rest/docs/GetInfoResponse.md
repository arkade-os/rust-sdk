# GetInfoResponse

## Properties

| Name                      | Type                                                             | Description                                               | Notes      |
| ------------------------- | ---------------------------------------------------------------- | --------------------------------------------------------- | ---------- |
| **boarding_exit_delay**   | Option<**i64**>                                                  |                                                           | [optional] |
| **checkpoint_tapscript**  | Option<**String**>                                               |                                                           | [optional] |
| **deprecated_signers**    | Option<[**Vec<models::DeprecatedSigner>**](DeprecatedSigner.md)> |                                                           | [optional] |
| **digest**                | Option<**String**>                                               |                                                           | [optional] |
| **dust**                  | Option<**i64**>                                                  |                                                           | [optional] |
| **fees**                  | Option<[**models::FeeInfo**](FeeInfo.md)>                        |                                                           | [optional] |
| **forfeit_address**       | Option<**String**>                                               |                                                           | [optional] |
| **forfeit_pubkey**        | Option<**String**>                                               |                                                           | [optional] |
| **network**               | Option<**String**>                                               |                                                           | [optional] |
| **scheduled_session**     | Option<[**models::ScheduledSession**](ScheduledSession.md)>      |                                                           | [optional] |
| **service_status**        | Option<**std::collections::HashMap<String, String>**>            |                                                           | [optional] |
| **session_duration**      | Option<**i64**>                                                  |                                                           | [optional] |
| **signer_pubkey**         | Option<**String**>                                               |                                                           | [optional] |
| **unilateral_exit_delay** | Option<**i64**>                                                  |                                                           | [optional] |
| **utxo_max_amount**       | Option<**i64**>                                                  | -1 means no limit (default), 0 means boarding not allowed | [optional] |
| **utxo_min_amount**       | Option<**i64**>                                                  |                                                           | [optional] |
| **version**               | Option<**String**>                                               |                                                           | [optional] |
| **vtxo_max_amount**       | Option<**i64**>                                                  | -1 means no limit (default)                               | [optional] |
| **vtxo_min_amount**       | Option<**i64**>                                                  |                                                           | [optional] |

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)
