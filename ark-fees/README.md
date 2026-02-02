# ark-fees

Fee estimation library using CEL (Common Expression Language) for calculating Arkade transaction fees.

> **Note:** This is a Rust port of the Go implementation at
> [`arkd/pkg/ark-lib/arkfee`](https://github.com/arkade-os/arkd/tree/master/pkg/ark-lib/arkfee).

## Overview

This crate provides an `Estimator` that evaluates CEL expressions to calculate fees based on input and output characteristics. Each estimator can have four separate programs:

- **Intent Offchain Input Program**: Evaluated for each offchain input (VTXO) in an intent
- **Intent Onchain Input Program**: Evaluated for each onchain input (boarding) in an intent
- **Intent Offchain Output Program**: Evaluated for each offchain output (VTXO) in an intent
- **Intent Onchain Output Program**: Evaluated for each onchain output (collaborative exit) in an intent

The total fee is the sum of all input fees plus all output fees.

## CEL Language

CEL (Common Expression Language) is a non-Turing complete expression language designed to be fast, portable, and safe. For the full language specification, see:
https://github.com/google/cel-spec/blob/master/doc/langdef.md

This crate uses [cel-rust](https://github.com/cel-rust/cel-rust) as the CEL implementation.

## CEL Environments

The library provides three CEL environments, each with their own set of available variables.

### Offchain Input Environment

Used for evaluating offchain input (VTXO) fee calculations.

| Variable    | Type     | Description                                               |
| ----------- | -------- | --------------------------------------------------------- |
| `amount`    | `double` | Amount in satoshis                                        |
| `expiry`    | `double` | Expiry date in Unix timestamp seconds                     |
| `birth`     | `double` | Birth date in Unix timestamp seconds                      |
| `weight`    | `double` | Weighted liquidity lockup ratio of a VTXO                 |
| `inputType` | `string` | Type of the input: `'vtxo'`, `'recoverable'`, or `'note'` |

### Onchain Input Environment

Used for evaluating onchain input (boarding) fee calculations.

| Variable | Type     | Description        |
| -------- | -------- | ------------------ |
| `amount` | `double` | Amount in satoshis |

### Output Environment

Used for evaluating output fee calculations (both offchain and onchain).

| Variable | Type     | Description          |
| -------- | -------- | -------------------- |
| `amount` | `double` | Amount in satoshis   |
| `script` | `string` | Hex encoded pkscript |

## Available Functions

All environments provide the following functions:

### `now() -> double`

Returns the current Unix timestamp in seconds.

## Usage

### Creating an Estimator

```rust
use ark_fees::{Config, Estimator};

let config = Config {
    intent_offchain_input_program: "weight * 0.01 * amount".to_string(),
    intent_onchain_input_program: "100.0".to_string(),
    intent_offchain_output_program: "amount * 0.002".to_string(),
    intent_onchain_output_program: "150.0".to_string(),
};

let estimator = Estimator::new(config)?;
```

All programs are optional. If a program is empty, the corresponding fee evaluation returns 0.

Programs are validated at construction time - if a program has syntax errors, references undefined variables, or returns a non-double type, `Estimator::new()` will return an error.

### Evaluating Fees

```rust
use ark_fees::{Estimator, OffchainInput, OnchainInput, Output, VtxoType};

// Evaluate fee for a single offchain input
let fee = estimator.eval_offchain_input(OffchainInput {
    amount: 10000,
    expiry: Some(now + 3600),
    birth: Some(now - 600),
    input_type: VtxoType::Vtxo,
    weight: 1.0,
})?;

// Evaluate fee for a single onchain input
let fee = estimator.eval_onchain_input(OnchainInput {
    amount: 5000,
})?;

// Evaluate fee for a single output
let fee = estimator.eval_offchain_output(Output {
    amount: 3000,
    script: "0014...".to_string(),
})?;

// Evaluate total fee for multiple inputs and outputs
let total_fee = estimator.eval(
    &offchain_inputs,
    &onchain_inputs,
    &offchain_outputs,
    &onchain_outputs,
)?;

// Convert to satoshis (rounds up)
let sats = total_fee.to_satoshis();
```

## Example Programs

### Offchain Input Programs

**Free for recoverable inputs:**

```cel
inputType == 'recoverable' ? 0.0 : 200.0
```

**Weighted fee (1% of amount):**

```cel
weight * 0.01 * amount
```

**Time-based fee (free if expires in less than 5 minutes):**

```cel
expiry - now() < 300.0 ? 0.0 : amount / 2.0
```

### Onchain Input Programs

**Fixed fee per boarding input:**

```cel
200.0
```

**Percentage fee (0.1% of amount):**

```cel
amount * 0.001
```

### Output Programs

**Fixed fee per output:**

```cel
100.0
```

**Percentage fee:**

```cel
amount * 0.002
```

**Fee based on script size:**

```cel
double(size(script)) * 0.01
```

## Return Type

All CEL programs must return a `double` (floating-point number) representing the fee amount in satoshis. Programs returning other types (int, string, bool) will fail validation at construction time.
