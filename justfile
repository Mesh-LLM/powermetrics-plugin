# Cargo names the host binary with .exe on Windows.
exe := if os_family() == "windows" { ".exe" } else { "" }

build:
    cargo build --locked

verify:
    cargo fmt --all --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_*.py'

# Build a release binary and package it for this machine's target triple.
package target:
    cargo build --locked --release
    python3 scripts/package.py target/release/powermetrics-plugin{{exe}} '{{target}}'

clean:
    cargo clean
