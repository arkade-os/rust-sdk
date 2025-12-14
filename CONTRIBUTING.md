# Contributing to ark-rs

Thank you for your interest in contributing to ark-rs! This guide will help you find good places to start contributing.

## Quick Start

1. **Set up your development environment** (see [README.md](README.md#local-development-setup))
2. **Run the tests** to make sure everything works: `just test`
3. **Pick an area** from the suggestions below
4. **Submit a Pull Request** when you're ready!

## Good First Contributions

### 🟢 Beginner-Friendly

#### 1. **Add a CONTRIBUTING.md file** ✅ (This file!)
- Status: Just created! But you can improve it with more details, examples, or guidelines.

#### 2. **Improve Documentation**
- Add more examples to the README
- Add doc comments to public APIs
- Create example code snippets for common use cases
- Document error types and when they occur

#### 3. **Implement `broadcast_package` in sample client**
- **Location**: `ark-client-sample/src/main.rs:504`
- **Current state**: `unimplemented!("Not implemented yet")`
- **What to do**: Implement package transaction broadcasting for the sample Esplora client
- **Difficulty**: Easy-Medium

#### 4. **Fix REST API deepObject style support**
- **Location**: `ark-rest/src/apis/mod.rs:92`
- **Current state**: `unimplemented!("Only objects are supported with style=deepObject")`
- **What to do**: Implement support for non-object types with deepObject style
- **Difficulty**: Medium (requires understanding OpenAPI parameter serialization)

### 🟡 Intermediate

#### 5. **Improve Coin Selection Algorithm**
- **Location**: `ark-client/src/coin_select.rs:21-24`
- **Current state**: Basic coin selection that doesn't account for fees
- **What to do**: Integrate a proper coin selection algorithm (e.g., from [bitcoindevkit/coin-select](https://github.com/bitcoindevkit/coin-select))
- **Why it matters**: Better fee estimation and UTXO selection
- **Difficulty**: Medium-Hard (requires understanding Bitcoin transaction fees)

#### 6. **Extract Coin Selection Logic to ark-core**
- **Location**: `ark-client/src/coin_select.rs:24`
- **Current state**: TODO comment suggests moving logic to `ark-core`
- **What to do**: Refactor coin selection to be reusable in `ark-core`
- **Difficulty**: Medium (requires understanding module boundaries)

#### 7. **Expand Boltz Test Coverage**
- **Locations**: 
  - `e2e-tests/tests/boltz_reverse.rs:14`
  - `e2e-tests/tests/boltz_submarine.rs:15`
- **Current state**: Tests require manual intervention and are marked `#[ignore]`
- **What to do**: Automate the tests by integrating with Lightning APIs directly
- **Difficulty**: Medium-Hard (requires Lightning network knowledge)

### 🔴 Advanced

#### 8. **Implement VHTLC Refund with Boltz Collaboration**
- **Location**: `ark-client/src/boltz.rs:588-589`
- **Current state**: `TODO: This path is not supported by Boltz yet.`
- **What to do**: Implement refund functionality once Boltz API supports it
- **Difficulty**: Hard (depends on external API support)

#### 9. **WASM Build Support**
- **Location**: `justfile:21-22`
- **Current state**: Only `ark-core` and `ark-rest` build for WASM
- **What to do**: Add WASM support for `ark-bdk-wallet` and eventually `ark-client`
- **Difficulty**: Hard (requires WASM compatibility knowledge)

## Finding More Opportunities

### Search for TODOs
```bash
grep -r "TODO" --include="*.rs" .
```

### Search for Unimplemented Code
```bash
grep -r "unimplemented!" --include="*.rs" .
```

### Check Test Coverage
- Look for `#[ignore]` tests that could be enabled
- Add unit tests for untested functions
- Improve error case testing

### Code Quality Improvements
- Run `just clippy` and fix any warnings
- Run `just fmt` to ensure consistent formatting
- Look for code that could be simplified or refactored

## Development Workflow

1. **Fork the repository**
2. **Create a feature branch**: `git checkout -b feature/your-feature-name`
3. **Make your changes**
4. **Run tests**: `just test`
5. **Run clippy**: `just clippy`
6. **Format code**: `just fmt`
7. **Commit your changes**: Write clear commit messages
8. **Push to your fork**: `git push origin feature/your-feature-name`
9. **Open a Pull Request**

## Code Style

- Follow Rust conventions
- Use `dprint` for formatting (run `just fmt`)
- Run `just clippy` before submitting PRs
- Add doc comments for public APIs
- Write tests for new functionality

## Getting Help

- Check existing issues and PRs
- Ask questions in discussions or issues
- Review the codebase to understand patterns

## Areas That Need Attention

Based on the codebase analysis:

1. **Error Handling**: Some error types could be more descriptive
2. **Documentation**: Many public APIs could use more examples
3. **Testing**: Some e2e tests require manual setup
4. **Code Organization**: Some logic could be better modularized

## Questions?

Feel free to open an issue or start a discussion if you need help getting started!

