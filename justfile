default:
    @just --list

build:
    cargo build --workspace --release --locked

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

check: fmt-check lint test

check-ci: check deny package
    python3 bench/bench.py --quick > /dev/null

package:
    cargo package --workspace --locked --allow-dirty

# `just package` can verify against a stale unpack of the workspace crates after an API change; this clears it.
clean-package:
    rm -rf ~/.cargo/registry/src/-*/witnessed-store-* ~/.cargo/registry/src/-*/witnessed-supervisor-*
    cargo clean -p witnessed-store -p witnessed-supervisor

deny:
    cargo deny check

bench:
    python3 bench/bench.py --json bench-results.json
