#!/usr/bin/env bash
# Local/CI gate: formatting, lints, tests.
#
# Set TEST_DATABASE_URL to a THROWAWAY PostgreSQL database to also run the
# repository and end-to-end suites; without it they skip.
set -euo pipefail

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
