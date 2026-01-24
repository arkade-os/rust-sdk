# Flutter Wallet Example

This example shows how to build a simple Flutter wallet application using the Rust SDK.

The Rust side exposes a small FFI layer compiled as a dynamic library using `cdylib`. The
Flutter application calls into this library using `dart:ffi`.

The default Ark server is **https://mutinynet.arkade.sh**. The example connects to this
server when initializing the client.

## Structure

- `lib/main.dart` – minimal Flutter UI that loads the dynamic library and displays an
  off‑chain address from the wallet.
- `native` – Rust crate compiled as a dynamic library. It wraps `ark-client` and provides
  FFI bindings.

## Building

```
flutter run
```

Ensure you have Rust and Flutter installed. The Rust crate builds as part of the Flutter
project via `cargo`.

