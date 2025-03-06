{
  description = "Development shell for ark-rs";

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    nixpkgs-stable.url = "github:NixOS/nixpkgs/nixos-24.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, nixpkgs-stable, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          config = {
            allowUnfree = true;
          };
        };
        pkgs-stable = import nixpkgs-stable { inherit system; };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustToolchainWithWasm = rustToolchain.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" ];
        };

        rustBinNightly = (pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.minimal)).override {
          extensions = [ "rustfmt" "rust-analyzer" ];
        };

        # From nightly, we only want to use rusfmt and rust-analyzer. The rest of rustBinNightly is ignored.
        rustfmt = rustBinNightly.passthru.availableComponents.rustfmt;
        rustAnalyzer = rustBinNightly.passthru.availableComponents.rust-analyzer;
      in
        {
          devShells.default = with pkgs; mkShell {
            # TODO: Trim these.
            buildInputs = [
              binaryen
              gcc
              go
              jq
              llvmPackages_latest.bintools
              openssl
              pkg-config
              postgresql
              rustAnalyzer
              # Must appear _before_ `rustToolchainWithWasm`.
              rustfmt
              rustToolchainWithWasm
              wabt
              wasm-bindgen-cli
              wasm-pack
              worker-build
            ];

            RUST_SRC_PATH = "${rustToolchainWithWasm}/lib/rustlib/src/rust/library";
          };
        }
    );
}
