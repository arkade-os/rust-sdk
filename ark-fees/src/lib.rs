//! Fee estimation library using CEL (Common Expression Language) for calculating Arkade transaction
//! fees.
//!
//! This library provides an `Estimator` that evaluates CEL expressions to calculate fees
//! based on input and output characteristics.

use cel::Context;
use cel::Program;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Fee amount as a floating point value in satoshis.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FeeAmount(pub f64);

impl FeeAmount {
    /// Converts the fee amount to satoshis, rounding up.
    pub fn to_satoshis(&self) -> u64 {
        self.0.max(0.0).ceil() as u64
    }
}

impl std::ops::Add for FeeAmount {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        FeeAmount(self.0 + other.0)
    }
}

impl std::ops::AddAssign for FeeAmount {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl From<f64> for FeeAmount {
    fn from(value: f64) -> Self {
        FeeAmount(value)
    }
}

/// Type of VTXO input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VtxoType {
    #[default]
    Vtxo,
    Recoverable,
    Note,
}

impl VtxoType {
    /// Returns the string representation used in CEL expressions.
    pub fn as_str(&self) -> &'static str {
        match self {
            VtxoType::Vtxo => "vtxo",
            VtxoType::Recoverable => "recoverable",
            VtxoType::Note => "note",
        }
    }
}

impl std::str::FromStr for VtxoType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vtxo" => Ok(VtxoType::Vtxo),
            "recoverable" => Ok(VtxoType::Recoverable),
            "note" => Ok(VtxoType::Note),
            _ => Err(format!("unknown vtxo type: {}", s)),
        }
    }
}

/// An offchain input (VTXO) for fee calculation.
#[derive(Debug, Clone, Default)]
pub struct OffchainInput {
    /// Amount in satoshis.
    pub amount: u64,
    /// Expiry time as Unix timestamp in seconds (optional).
    pub expiry: Option<i64>,
    /// Birth time as Unix timestamp in seconds (optional).
    pub birth: Option<i64>,
    /// Type of the input.
    pub input_type: VtxoType,
    /// Weighted liquidity lockup ratio.
    pub weight: f64,
}

/// An onchain input (boarding) for fee calculation.
#[derive(Debug, Clone, Default)]
pub struct OnchainInput {
    /// Amount in satoshis.
    pub amount: u64,
}

/// An output for fee calculation.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// Amount in satoshis.
    pub amount: u64,
    /// Hex encoded pkscript.
    pub script: String,
}

/// Configuration for the fee estimator.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// CEL program for offchain input fees.
    pub intent_offchain_input_program: String,
    /// CEL program for onchain input fees.
    pub intent_onchain_input_program: String,
    /// CEL program for offchain output fees.
    pub intent_offchain_output_program: String,
    /// CEL program for onchain output fees.
    pub intent_onchain_output_program: String,
}

/// A compiled CEL program that can be evaluated.
struct CompiledProgram {
    program: Program,
    #[allow(dead_code)]
    source: String,
}

impl std::fmt::Debug for CompiledProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledProgram")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Fee estimator using CEL expressions.
#[derive(Debug)]
pub struct Estimator {
    intent_offchain_input: Option<CompiledProgram>,
    intent_onchain_input: Option<CompiledProgram>,
    intent_offchain_output: Option<CompiledProgram>,
    intent_onchain_output: Option<CompiledProgram>,
}

/// Error type for fee estimation.
#[derive(Debug)]
pub enum Error {
    /// Error compiling CEL program.
    Compile(String),
    /// Error evaluating CEL program.
    Eval(String),
    /// Unexpected return type from CEL program.
    ReturnType(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Compile(msg) => write!(f, "compile error: {}", msg),
            Error::Eval(msg) => write!(f, "eval error: {}", msg),
            Error::ReturnType(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {}

/// Returns the current Unix timestamp in seconds.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64
}

/// Environment type for program validation.
#[derive(Debug, Clone, Copy)]
enum ProgramType {
    OffchainInput,
    OnchainInput,
    Output,
}

/// Compiles a CEL program and validates it returns a numeric type.
fn compile_program(source: &str, program_type: ProgramType) -> Result<CompiledProgram, Error> {
    let program = Program::compile(source).map_err(|e| Error::Compile(format!("{}", e)))?;

    // Validate by doing a dry run with dummy values
    let context = create_validation_context(program_type);
    let result = program
        .execute(&context)
        .map_err(|e| Error::Compile(format!("{}", e)))?;

    // Verify the return type is double (float)
    // We strictly require Float to match cel-go's behavior
    match result {
        cel::Value::Float(_) => {}
        cel::Value::Int(_)
        | cel::Value::UInt(_)
        | cel::Value::String(_)
        | cel::Value::Bytes(_)
        | cel::Value::Bool(_)
        | cel::Value::List(_)
        | cel::Value::Map(_)
        | cel::Value::Null
        | cel::Value::Duration(_)
        | cel::Value::Timestamp(_)
        | cel::Value::Function(_, _)
        | cel::Value::Opaque(_) => {
            return Err(Error::ReturnType(format!(
                "expected return type double, got {:?}",
                result
            )));
        }
    }

    Ok(CompiledProgram {
        program,
        source: source.to_string(),
    })
}

/// Creates a context with dummy values for program validation.
fn create_validation_context(program_type: ProgramType) -> Context<'static> {
    let mut context = Context::default();

    match program_type {
        ProgramType::OffchainInput => {
            let _ = context.add_variable("amount", 0.0_f64);
            let _ = context.add_variable("inputType", "vtxo");
            let _ = context.add_variable("weight", 0.0_f64);
            let _ = context.add_variable("expiry", 0.0_f64);
            let _ = context.add_variable("birth", 0.0_f64);
        }
        ProgramType::OnchainInput => {
            let _ = context.add_variable("amount", 0.0_f64);
        }
        ProgramType::Output => {
            let _ = context.add_variable("amount", 0.0_f64);
            let _ = context.add_variable("script", "");
        }
    }

    context.add_function("now", now);
    context
}

/// Creates a CEL context for offchain input evaluation.
fn create_offchain_input_context(input: &OffchainInput) -> Context<'static> {
    let mut context = Context::default();
    let _ = context.add_variable("amount", input.amount as f64);
    let _ = context.add_variable("inputType", input.input_type.as_str());
    let _ = context.add_variable("weight", input.weight);

    if let Some(expiry) = input.expiry {
        let _ = context.add_variable("expiry", expiry as f64);
    }
    if let Some(birth) = input.birth {
        let _ = context.add_variable("birth", birth as f64);
    }

    context.add_function("now", now);
    context
}

/// Creates a CEL context for onchain input evaluation.
fn create_onchain_input_context(input: &OnchainInput) -> Context<'static> {
    let mut context = Context::default();
    let _ = context.add_variable("amount", input.amount as f64);
    context.add_function("now", now);
    context
}

/// Creates a CEL context for output evaluation.
fn create_output_context(output: &Output) -> Context<'static> {
    let mut context = Context::default();
    let _ = context.add_variable("amount", output.amount as f64);
    let _ = context.add_variable("script", output.script.clone());
    context.add_function("now", now);
    context
}

impl Estimator {
    /// Creates a new fee estimator from the given configuration.
    ///
    /// Programs are optional; if empty, the corresponding fee evaluation returns 0.
    pub fn new(config: Config) -> Result<Self, Error> {
        let intent_offchain_input = if !config.intent_offchain_input_program.is_empty() {
            Some(compile_program(
                &config.intent_offchain_input_program,
                ProgramType::OffchainInput,
            )?)
        } else {
            None
        };

        let intent_onchain_input = if !config.intent_onchain_input_program.is_empty() {
            Some(compile_program(
                &config.intent_onchain_input_program,
                ProgramType::OnchainInput,
            )?)
        } else {
            None
        };

        let intent_offchain_output = if !config.intent_offchain_output_program.is_empty() {
            Some(compile_program(
                &config.intent_offchain_output_program,
                ProgramType::Output,
            )?)
        } else {
            None
        };

        let intent_onchain_output = if !config.intent_onchain_output_program.is_empty() {
            Some(compile_program(
                &config.intent_onchain_output_program,
                ProgramType::Output,
            )?)
        } else {
            None
        };

        Ok(Estimator {
            intent_offchain_input,
            intent_onchain_input,
            intent_offchain_output,
            intent_onchain_output,
        })
    }

    /// Evaluates the fee for a given offchain input (VTXO).
    pub fn eval_offchain_input(&self, input: OffchainInput) -> Result<FeeAmount, Error> {
        match &self.intent_offchain_input {
            Some(compiled) => {
                let context = create_offchain_input_context(&input);
                let result = compiled
                    .program
                    .execute(&context)
                    .map_err(|e| Error::Eval(format!("{}", e)))?;

                match result {
                    cel::Value::Float(f) => Ok(FeeAmount(f)),
                    cel::Value::Int(i) => Ok(FeeAmount(i as f64)),
                    cel::Value::UInt(u) => Ok(FeeAmount(u as f64)),
                    cel::Value::String(_)
                    | cel::Value::Bytes(_)
                    | cel::Value::Bool(_)
                    | cel::Value::List(_)
                    | cel::Value::Map(_)
                    | cel::Value::Null
                    | cel::Value::Duration(_)
                    | cel::Value::Timestamp(_)
                    | cel::Value::Function(_, _)
                    | cel::Value::Opaque(_) => Err(Error::ReturnType(format!(
                        "expected return type double, got {:?}",
                        result
                    ))),
                }
            }
            None => Ok(FeeAmount(0.0)),
        }
    }

    /// Evaluates the fee for a given onchain input (boarding).
    pub fn eval_onchain_input(&self, input: OnchainInput) -> Result<FeeAmount, Error> {
        match &self.intent_onchain_input {
            Some(compiled) => {
                let context = create_onchain_input_context(&input);
                let result = compiled
                    .program
                    .execute(&context)
                    .map_err(|e| Error::Eval(format!("{}", e)))?;

                match result {
                    cel::Value::Float(f) => Ok(FeeAmount(f)),
                    cel::Value::Int(i) => Ok(FeeAmount(i as f64)),
                    cel::Value::UInt(u) => Ok(FeeAmount(u as f64)),
                    cel::Value::String(_)
                    | cel::Value::Bytes(_)
                    | cel::Value::Bool(_)
                    | cel::Value::List(_)
                    | cel::Value::Map(_)
                    | cel::Value::Null
                    | cel::Value::Duration(_)
                    | cel::Value::Timestamp(_)
                    | cel::Value::Function(_, _)
                    | cel::Value::Opaque(_) => Err(Error::ReturnType(format!(
                        "expected return type double, got {:?}",
                        result
                    ))),
                }
            }
            None => Ok(FeeAmount(0.0)),
        }
    }

    /// Evaluates the fee for a given offchain output (VTXO).
    pub fn eval_offchain_output(&self, output: Output) -> Result<FeeAmount, Error> {
        match &self.intent_offchain_output {
            Some(compiled) => {
                let context = create_output_context(&output);
                let result = compiled
                    .program
                    .execute(&context)
                    .map_err(|e| Error::Eval(format!("{}", e)))?;

                match result {
                    cel::Value::Float(f) => Ok(FeeAmount(f)),
                    cel::Value::Int(i) => Ok(FeeAmount(i as f64)),
                    cel::Value::UInt(u) => Ok(FeeAmount(u as f64)),
                    cel::Value::String(_)
                    | cel::Value::Bytes(_)
                    | cel::Value::Bool(_)
                    | cel::Value::List(_)
                    | cel::Value::Map(_)
                    | cel::Value::Null
                    | cel::Value::Duration(_)
                    | cel::Value::Timestamp(_)
                    | cel::Value::Function(_, _)
                    | cel::Value::Opaque(_) => Err(Error::ReturnType(format!(
                        "expected return type double, got {:?}",
                        result
                    ))),
                }
            }
            None => Ok(FeeAmount(0.0)),
        }
    }

    /// Evaluates the fee for a given onchain output (collaborative exit).
    pub fn eval_onchain_output(&self, output: Output) -> Result<FeeAmount, Error> {
        match &self.intent_onchain_output {
            Some(compiled) => {
                let context = create_output_context(&output);
                let result = compiled
                    .program
                    .execute(&context)
                    .map_err(|e| Error::Eval(format!("{}", e)))?;

                match result {
                    cel::Value::Float(f) => Ok(FeeAmount(f)),
                    cel::Value::Int(i) => Ok(FeeAmount(i as f64)),
                    cel::Value::UInt(u) => Ok(FeeAmount(u as f64)),
                    cel::Value::String(_)
                    | cel::Value::Bytes(_)
                    | cel::Value::Bool(_)
                    | cel::Value::List(_)
                    | cel::Value::Map(_)
                    | cel::Value::Null
                    | cel::Value::Duration(_)
                    | cel::Value::Timestamp(_)
                    | cel::Value::Function(_, _)
                    | cel::Value::Opaque(_) => Err(Error::ReturnType(format!(
                        "expected return type double, got {:?}",
                        result
                    ))),
                }
            }
            None => Ok(FeeAmount(0.0)),
        }
    }

    /// Evaluates the total fee for a given set of inputs and outputs.
    pub fn eval(
        &self,
        offchain_inputs: &[OffchainInput],
        onchain_inputs: &[OnchainInput],
        offchain_outputs: &[Output],
        onchain_outputs: &[Output],
    ) -> Result<FeeAmount, Error> {
        let mut fee = FeeAmount(0.0);

        for input in offchain_inputs {
            fee += self.eval_offchain_input(input.clone())?;
        }

        for input in onchain_inputs {
            fee += self.eval_onchain_input(input.clone())?;
        }

        for output in offchain_outputs {
            fee += self.eval_offchain_output(output.clone())?;
        }

        for output in onchain_outputs {
            fee += self.eval_onchain_output(output.clone())?;
        }

        Ok(fee)
    }
}
