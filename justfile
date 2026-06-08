# JMAP-Matrix Bridge Development Tasks

# Default: check code and run tests
default: check test

# Run all tests via standard cargo
test:
    cargo test-all

# Run all tests via cargo-nextest (ultra fast, parallelized!)
nextest:
    cargo nextest-all

# Check compilation
check:
    cargo check-all

# Run the bridge in development mode (requires config.yaml)
run:
    cargo run -- --config config.yaml

# Lint code thoroughly (library, binary, tests, examples)
lint:
    cargo clippy-all
    cargo fmt --all -- --check

# Continuous background check using bacon (instantly updates on edits)
bacon:
    bacon

# Fix common issues
fix:
    cargo clippy --fix --allow-dirty --allow-staged
    cargo fmt

# Update dependencies
update:
    cargo update

# Clean build artifacts
clean:
    cargo clean

