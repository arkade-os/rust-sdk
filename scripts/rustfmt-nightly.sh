#!/usr/bin/env bash
# Wrapper for dprint exec: use nightly rustfmt via rustup if available,
# otherwise fall back to whatever rustfmt is on PATH (e.g. Nix-provided).
if rustup run nightly-2026-03-07 rustfmt --version &>/dev/null; then
    exec rustup run nightly-2026-03-07 rustfmt "$@"
else
    exec rustfmt "$@"
fi
