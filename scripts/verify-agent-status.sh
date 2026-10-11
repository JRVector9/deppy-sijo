#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_TEST_THREADS=1
run_cargo() {
  if [[ -n "${DEPPY_CARGO_GATE:-}" ]]; then
    python3 "$DEPPY_CARGO_GATE" "$@"
  else
    cargo "$@"
  fi
}
extra=()
if [[ "${DEPPY_CARGO_OFFLINE:-0}" == "1" ]]; then extra+=(--offline); fi
# One corpus for local runs. CI runs the same scopes as the named
# "Agent status ownership and provider replay" step (AGENT_STATUS_TESTS in
# .github/workflows/build-test.yml); keep the two in sync.
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p storage
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p mcp-proxy -- --nocapture
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p session status::tests
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p deppy-sijo --bin deppy-sijo agent_shim::tests
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p deppy-sijo --bin deppy-sijo merge_agent_status
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p web-remote --lib agent_status_harness
run_cargo test --locked ${extra[@]+"${extra[@]}"} -q -p runtime agent_status_harness
