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

deny:
    cargo deny check

bench:
    python3 bench/bench.py --json bench-results.json
