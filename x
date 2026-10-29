#!/bin/sh
# One repository entry point. Keep CI, contributor docs, and local checks on the
# same commands so no environment has a private definition of "green".
set -eu
cd "$(dirname "$0")"

# Print the command contract; errors exit 2 and explicit help exits 0.
usage() {
    cat <<'EOF'
Usage: ./x COMMAND [args...]

  build   build the development binary
  fmt     format Rust (add --check to inspect only)
  lint    run Clippy with warnings denied
  test    run package tests
  check   format check, lint, tests, shell syntax, and security guard (default)
  shell   check shell-script syntax
  guard   audit the security surface
  eval    score labeled retrieval (--packets for replay, --binary for live)
  serve   build and run paired HTTPS hosting
  stdio   build and run MCP over stdio
  help    show this reference
EOF
    exit "${1:-2}"
}

command=${1-check}
if [ "$#" -gt 0 ]; then
    shift
fi

case "$command" in
    help|-h|--help)
        [ "$#" -eq 0 ] || usage >&2
        usage 0
        ;;
    build)
        cargo build --locked "$@"
        ;;
    fmt)
        cargo fmt --all "$@"
        ;;
    lint)
        cargo clippy --locked --all-targets --all-features "$@" -- -D warnings
        ;;
    test)
        cargo test --locked "$@"
        ;;
    check)
        [ "$#" -eq 0 ] || usage >&2
        ./x fmt --check
        ./x lint
        cargo check --locked --no-default-features
        cargo check --locked --no-default-features --features mcp
        cargo check --locked --no-default-features --test public_modules
        cargo check --locked --no-default-features --features mcp --test public_modules
        ./x test
        PYTHONDONTWRITEBYTECODE=1 python3 tests/evaluate.py
        ./x shell
        ./x guard
        python3 -m unittest discover -s scripts -p "test_*.py"
        ;;
    guard)
        [ "$#" -eq 0 ] || usage >&2
        sh scripts/guard.sh
        ;;
    eval)
        PYTHONDONTWRITEBYTECODE=1 exec python3 scripts/evaluate.py "$@"
        ;;
    # The shell scripts the release runs: a syntax error here fails a release,
    # not a pull request, so it belongs in check.
    shell)
        [ "$#" -eq 0 ] || usage >&2
        for script in x install.sh scripts/*.sh; do
            sh -n "$script"
        done
        echo "shell: ok"
        ;;
    serve)
        cargo build --locked
        exec ./target/debug/search serve "$@"
        ;;
    stdio)
        cargo build --locked
        exec ./target/debug/search "$@"
        ;;
    *)
        usage >&2
        ;;
esac
