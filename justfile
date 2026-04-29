set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe: run all checks
default: fmt build test

# Check formatting without modifying files
fmt:
    cargo fmt -- --check

# Build the tau-agent crate (binary still named 'tau')
build:
    cargo build -p tau-agent

# Run the full workspace test suite.
#
# Parallelism is intentionally capped to keep machine-resource pressure
# bounded. The `tau-agent-lib` package has 8 integration test binaries
# (`e2e_*.rs`, `plugin_test.rs`, `shutdown_kills_bash.rs`); each spins up
# multiple `TestServer` instances. With cargo's defaults that's up to
# ~(num_binaries × nproc) simultaneous servers, which produces intermittent
# timeouts on loaded developer machines (e.g. when two agents run
# `just test` concurrently — see tasks #896 / #897).
#
# The two knobs:
#   -j 2            : at most 2 cargo test binaries running concurrently.
#   --test-threads=4: at most 4 threads inside each binary.
# Product: ≤8 simultaneous `TestServer`s, which the harness sustains.
#
# Revisit if the e2e binaries are consolidated, or if the harness gains a
# shared server fixture. CI-specific tuning is a separate concern.
test:
    cargo test --workspace -j 2 -- --test-threads=4
