// Allow common test patterns that clippy warns about
#![allow(clippy::unwrap_used, clippy::expect_fun_call)]

use ark_fees::Config;
use ark_fees::Estimator;
use ark_fees::FeeAmount;
use ark_fees::OffchainInput;
use ark_fees::OnchainInput;
use ark_fees::Output;
use ark_fees::VtxoType;
use serde::Deserialize;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

// Test data structures matching the JSON format

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestData {
    eval_offchain_input: Vec<InputFixture>,
    eval_onchain_input: Vec<InputFixture>,
    eval_offchain_output: Vec<OutputFixture>,
    eval_onchain_output: Vec<OutputFixture>,
    eval: Vec<EvalFixture>,
}

#[derive(Debug, Deserialize)]
struct InputFixture {
    name: String,
    program: String,
    cases: Vec<InputCase>,
}

#[derive(Debug, Deserialize)]
struct InputCase {
    name: String,
    input: JsonInput,
    expected: f64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct JsonInput {
    #[serde(default)]
    amount: u64,
    birth_offset_seconds: Option<i64>,
    expiry_offset_seconds: Option<i64>,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    weight: f64,
}

#[derive(Debug, Deserialize)]
struct OutputFixture {
    name: String,
    program: String,
    cases: Vec<OutputCase>,
}

#[derive(Debug, Deserialize)]
struct OutputCase {
    name: String,
    output: JsonOutput,
    expected: f64,
}

#[derive(Debug, Deserialize, Default)]
struct JsonOutput {
    #[serde(default)]
    amount: u64,
    #[serde(default)]
    script: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalFixture {
    name: String,
    #[serde(default)]
    offchain_input_program: String,
    #[serde(default)]
    onchain_input_program: String,
    #[serde(default)]
    offchain_output_program: String,
    #[serde(default)]
    onchain_output_program: String,
    cases: Vec<EvalCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalCase {
    name: String,
    #[serde(default)]
    offchain_inputs: Vec<JsonInput>,
    #[serde(default)]
    onchain_inputs: Vec<JsonOnchainInput>,
    #[serde(default)]
    offchain_outputs: Vec<JsonOutput>,
    #[serde(default)]
    onchain_outputs: Vec<JsonOutput>,
    expected: f64,
}

#[derive(Debug, Deserialize, Default)]
struct JsonOnchainInput {
    #[serde(default)]
    amount: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvalidTestData {
    invalid_configs: Vec<InvalidConfig>,
}

#[derive(Debug, Deserialize)]
struct InvalidConfig {
    name: String,
    config: JsonConfig,
    #[allow(dead_code)]
    err: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct JsonConfig {
    #[serde(default)]
    offchain_input_program: String,
    #[serde(default)]
    onchain_input_program: String,
    #[serde(default)]
    offchain_output_program: String,
    #[serde(default)]
    onchain_output_program: String,
}

fn load_test_data() -> TestData {
    let data = include_str!("testdata/valid.json");
    serde_json::from_str(data).expect("Failed to parse test data")
}

fn load_invalid_test_data() -> InvalidTestData {
    let data = include_str!("testdata/invalid.json");
    serde_json::from_str(data).expect("Failed to parse invalid test data")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn convert_json_input(j: &JsonInput) -> OffchainInput {
    let now = now_unix();

    let birth = j.birth_offset_seconds.map(|offset| now + offset);
    let expiry = j.expiry_offset_seconds.map(|offset| now + offset);

    let input_type = if j.r#type.is_empty() {
        VtxoType::Vtxo
    } else {
        j.r#type.parse().unwrap_or(VtxoType::Vtxo)
    };

    OffchainInput {
        amount: j.amount,
        birth,
        expiry,
        input_type,
        weight: j.weight,
    }
}

fn convert_json_onchain_input(j: &JsonOnchainInput) -> OnchainInput {
    OnchainInput { amount: j.amount }
}

fn convert_json_output(j: &JsonOutput) -> Output {
    Output {
        amount: j.amount,
        script: j.script.clone(),
    }
}

#[test]
fn test_new_invalid() {
    let data = load_invalid_test_data();

    for test_case in data.invalid_configs {
        let config = Config {
            intent_offchain_input_program: test_case.config.offchain_input_program,
            intent_onchain_input_program: test_case.config.onchain_input_program,
            intent_offchain_output_program: test_case.config.offchain_output_program,
            intent_onchain_output_program: test_case.config.onchain_output_program,
        };

        let result = Estimator::new(config);
        assert!(
            result.is_err(),
            "Test '{}' should have failed but didn't",
            test_case.name
        );
    }
}

#[test]
fn test_new_wrong_return_type() {
    // Test that creating an estimator with a program that returns the wrong type fails
    let result = Estimator::new(Config {
        intent_offchain_input_program: "'hello'".to_string(),
        ..Default::default()
    });

    assert!(
        result.is_err(),
        "Should fail when program returns wrong type"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("expected return type double"),
        "Error should mention wrong return type, got: {}",
        err
    );
}

#[test]
fn test_new_undefined_variable() {
    // Test that using an undefined variable fails at construction time
    let result = Estimator::new(Config {
        intent_offchain_input_program: "undefinedVar * 2.0".to_string(),
        ..Default::default()
    });

    assert!(result.is_err(), "Should fail when using undefined variable");
}

#[test]
fn test_new_wrong_env_variable() {
    // Test that using a variable from the wrong environment fails
    // inputType is only available in offchain input, not onchain input
    let result = Estimator::new(Config {
        intent_onchain_input_program: "inputType == 'vtxo' ? 0.0 : 100.0".to_string(),
        ..Default::default()
    });

    assert!(
        result.is_err(),
        "Should fail when using variable from wrong environment"
    );
}

#[test]
fn test_eval_offchain_input_no_program() {
    let estimator = Estimator::new(Config::default()).unwrap();
    let result = estimator
        .eval_offchain_input(OffchainInput::default())
        .unwrap();
    assert_eq!(result, FeeAmount(0.0));
}

#[test]
fn test_eval_offchain_input() {
    let data = load_test_data();

    for fixture in data.eval_offchain_input {
        let estimator = Estimator::new(Config {
            intent_offchain_input_program: fixture.program.clone(),
            ..Default::default()
        })
        .expect(&format!(
            "Failed to create estimator for '{}'",
            fixture.name
        ));

        for test_case in fixture.cases {
            let input = convert_json_input(&test_case.input);
            let result = estimator.eval_offchain_input(input).expect(&format!(
                "Failed to eval '{}/{}'",
                fixture.name, test_case.name
            ));

            assert!(
                (result.0 - test_case.expected).abs() < 0.001,
                "Test '{}/{}': expected {}, got {}",
                fixture.name,
                test_case.name,
                test_case.expected,
                result.0
            );
        }
    }
}

#[test]
fn test_eval_onchain_input_no_program() {
    let estimator = Estimator::new(Config::default()).unwrap();
    let result = estimator
        .eval_onchain_input(OnchainInput::default())
        .unwrap();
    assert_eq!(result, FeeAmount(0.0));
}

#[test]
fn test_eval_onchain_input() {
    let data = load_test_data();

    for fixture in data.eval_onchain_input {
        let estimator = Estimator::new(Config {
            intent_onchain_input_program: fixture.program.clone(),
            ..Default::default()
        })
        .expect(&format!(
            "Failed to create estimator for '{}'",
            fixture.name
        ));

        for test_case in fixture.cases {
            let input = convert_json_onchain_input(&JsonOnchainInput {
                amount: test_case.input.amount,
            });
            let result = estimator.eval_onchain_input(input).expect(&format!(
                "Failed to eval '{}/{}'",
                fixture.name, test_case.name
            ));

            assert!(
                (result.0 - test_case.expected).abs() < 0.001,
                "Test '{}/{}': expected {}, got {}",
                fixture.name,
                test_case.name,
                test_case.expected,
                result.0
            );
        }
    }
}

#[test]
fn test_eval_offchain_output_no_program() {
    let estimator = Estimator::new(Config::default()).unwrap();
    let result = estimator.eval_offchain_output(Output::default()).unwrap();
    assert_eq!(result, FeeAmount(0.0));
}

#[test]
fn test_eval_offchain_output() {
    let data = load_test_data();

    for fixture in data.eval_offchain_output {
        let estimator = Estimator::new(Config {
            intent_offchain_output_program: fixture.program.clone(),
            ..Default::default()
        })
        .expect(&format!(
            "Failed to create estimator for '{}'",
            fixture.name
        ));

        for test_case in fixture.cases {
            let output = convert_json_output(&test_case.output);
            let result = estimator.eval_offchain_output(output).expect(&format!(
                "Failed to eval '{}/{}'",
                fixture.name, test_case.name
            ));

            assert!(
                (result.0 - test_case.expected).abs() < 0.001,
                "Test '{}/{}': expected {}, got {}",
                fixture.name,
                test_case.name,
                test_case.expected,
                result.0
            );
        }
    }
}

#[test]
fn test_eval_onchain_output_no_program() {
    let estimator = Estimator::new(Config::default()).unwrap();
    let result = estimator.eval_onchain_output(Output::default()).unwrap();
    assert_eq!(result, FeeAmount(0.0));
}

#[test]
fn test_eval_onchain_output() {
    let data = load_test_data();

    for fixture in data.eval_onchain_output {
        let estimator = Estimator::new(Config {
            intent_onchain_output_program: fixture.program.clone(),
            ..Default::default()
        })
        .expect(&format!(
            "Failed to create estimator for '{}'",
            fixture.name
        ));

        for test_case in fixture.cases {
            let output = convert_json_output(&test_case.output);
            let result = estimator.eval_onchain_output(output).expect(&format!(
                "Failed to eval '{}/{}'",
                fixture.name, test_case.name
            ));

            assert!(
                (result.0 - test_case.expected).abs() < 0.001,
                "Test '{}/{}': expected {}, got {}",
                fixture.name,
                test_case.name,
                test_case.expected,
                result.0
            );
        }
    }
}

#[test]
fn test_eval() {
    let data = load_test_data();

    for fixture in data.eval {
        let estimator = Estimator::new(Config {
            intent_offchain_input_program: fixture.offchain_input_program.clone(),
            intent_onchain_input_program: fixture.onchain_input_program.clone(),
            intent_offchain_output_program: fixture.offchain_output_program.clone(),
            intent_onchain_output_program: fixture.onchain_output_program.clone(),
        })
        .expect(&format!(
            "Failed to create estimator for '{}'",
            fixture.name
        ));

        for test_case in fixture.cases {
            let offchain_inputs: Vec<_> = test_case
                .offchain_inputs
                .iter()
                .map(convert_json_input)
                .collect();

            let onchain_inputs: Vec<_> = test_case
                .onchain_inputs
                .iter()
                .map(convert_json_onchain_input)
                .collect();

            let offchain_outputs: Vec<_> = test_case
                .offchain_outputs
                .iter()
                .map(convert_json_output)
                .collect();

            let onchain_outputs: Vec<_> = test_case
                .onchain_outputs
                .iter()
                .map(convert_json_output)
                .collect();

            let result = estimator
                .eval(
                    &offchain_inputs,
                    &onchain_inputs,
                    &offchain_outputs,
                    &onchain_outputs,
                )
                .expect(&format!(
                    "Failed to eval '{}/{}'",
                    fixture.name, test_case.name
                ));

            assert!(
                (result.0 - test_case.expected).abs() < 0.001,
                "Test '{}/{}': expected {}, got {}",
                fixture.name,
                test_case.name,
                test_case.expected,
                result.0
            );
        }
    }
}

#[test]
fn test_fee_amount_to_satoshis() {
    assert_eq!(FeeAmount(100.0).to_satoshis(), 100);
    assert_eq!(FeeAmount(100.1).to_satoshis(), 101);
    assert_eq!(FeeAmount(100.9).to_satoshis(), 101);
    assert_eq!(FeeAmount(0.0).to_satoshis(), 0);
    assert_eq!(FeeAmount(-0.0).to_satoshis(), 0);
}

#[test]
fn test_fee_amount_add() {
    let a = FeeAmount(100.0);
    let b = FeeAmount(50.5);
    let c = a + b;
    assert_eq!(c.0, 150.5);
}

#[test]
fn test_vtxo_type_as_str() {
    assert_eq!(VtxoType::Vtxo.as_str(), "vtxo");
    assert_eq!(VtxoType::Recoverable.as_str(), "recoverable");
    assert_eq!(VtxoType::Note.as_str(), "note");
}

#[test]
fn test_vtxo_type_from_str() {
    assert_eq!("vtxo".parse::<VtxoType>().unwrap(), VtxoType::Vtxo);
    assert_eq!(
        "recoverable".parse::<VtxoType>().unwrap(),
        VtxoType::Recoverable
    );
    assert_eq!("note".parse::<VtxoType>().unwrap(), VtxoType::Note);
    assert!("invalid".parse::<VtxoType>().is_err());
}
