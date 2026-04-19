set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe: run all checks
default: fmt build test

# Check formatting without modifying files
fmt:
    cargo fmt -- --check

# Build the tau-agent crate (binary still named 'tau')
build:
    cargo build -p tau-agent

# Run the full workspace test suite
test:
    cargo test --workspace
