# Ark REST Client

A low-level Rust HTTP client for the Ark protocol, providing access to both the main Ark service and indexer APIs
through a unified interface.

## Architecture

This crate consists of two main parts:

- **Generated Code** (`src/generated/`) - Auto-generated OpenAPI client code (don't edit manually)
- **Client Wrapper** (`src/client.rs`) - High-level client providing a clean, idiomatic Rust API

The generated code is created from merged OpenAPI/Swagger specifications that combine:

- `swagger/service.swagger.json` - Main Ark service API
- `swagger/indexer.swagger.json` - Indexer service API
- `swagger/types.swagger.json` - Shared type definitions

## Prerequisites

1. **Python 3** (for merging swagger files)
2. **Node.js and npm** (for OpenAPI generator)
3. **Just** (task runner) - `cargo install just`

Install the OpenAPI Generator CLI:

```bash
npm install @openapitools/openapi-generator-cli -g
```

## Quick Start

### 1. Generate the Client

```bash
# Generate client from latest swagger specs
just merge-swagger generate
```

This will:

- Merge the three swagger files into one
- Generate Rust client code
- Apply necessary fixes to the generated code
- Format the code

### 2. Build and Test

```bash
# Build the project
just build

# Run tests
just test
```

### 3. Use the Client

```rust
use ark_rest::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a new client
    let client = Client::new("http://localhost:8080");

    // Get server info
    let info = client.get_info().await?;
    println!("Server version: {}", info.version);

    // List VTXOs
    let vtxos = client.list_vtxos(scripts, None, None).await?;
    println!("Found {} VTXOs", vtxos.vtxos.len());

    // Subscribe to script notifications
    let subscription_id = client
        .subscribe_to_scripts(scripts, "my-sub".to_string())
        .await?;
    println!("Created subscription: {}", subscription_id);

    Ok(())
}
```

## Available Commands

| Command              | Description                         |
| -------------------- | ----------------------------------- |
| `just`               | Show all available commands         |
| `just generate`      | Generate client from merged swagger |
| `just merge-swagger` | Merge swagger files only            |

## Updating the Client

When the Ark APIs change:

1. **Update swagger files** - Download latest specs to `swagger/` directory
2. **Regenerate client** - Run `just merge-swagger generate`
3. **Review changes** - Check generated code and update client wrapper if needed
4. **Test thoroughly** - Run `just wasm-test` from the project root to ensure everything works

## WASM Testing

The client includes WebAssembly compatibility tests that can be run in a browser environment.

### Prerequisites for WASM Tests

1. **wasm-pack** - Automatically installed by the just commands if missing
2. **Running Ark server** - Tests expect server on `http://localhost:7070`
3. **Browser** - Firefox or Chrome (headless mode)

### Running WASM Tests

```bash
# Run WASM tests with Firefox (default)
just test-wasm
```

The WASM tests verify that the client works correctly in a browser environment and can successfully communicate with the
Ark server.
