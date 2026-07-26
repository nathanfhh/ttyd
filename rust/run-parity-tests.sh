#!/usr/bin/env bash
#
# Runs the characterization suite twice: once against this Rust build, once against a C
# reference binary. Any test that passes for one and fails for the other is a behavioural
# divergence, which is the whole point of keeping both runs.
#
# Usage:
#   ./run-parity-tests.sh                       # Rust build only
#   ./run-parity-tests.sh /path/to/c/ttyd       # both, and compare
set -uo pipefail

# Without this guard a failed cd would silently run the whole suite from the caller's
# directory, against whatever binary happened to be there.
cd "$(dirname "$0")" || exit 1

C_BINARY="${1:-}"
SUITES=(cli_parity http_parity ws_parity tls_parity lifecycle_parity)
FAILED=0

run_suite_set() {
    local label="$1"
    shift
    echo
    echo "════════════════════════════════════════════════════════════"
    echo "  $label"
    echo "════════════════════════════════════════════════════════════"
    for suite in "${SUITES[@]}"; do
        printf '  %-16s ' "$suite"
        if output=$(cargo test --test "$suite" -- --test-threads=4 2>&1); then
            echo "$(echo "$output" | grep -E '^test result:' | head -1)"
        else
            echo "FAILED"
            echo "$output" | grep -E '^(test .* FAILED|    [a-z_]+$)' | head -20
            FAILED=1
        fi
    done
}

echo "Building..."
cargo build --tests --quiet || exit 1

# Unit tests and the forward-auth suite only apply to this implementation.
echo
echo "════════════════════════════════════════════════════════════"
echo "  Unit tests + forward authentication (this port only)"
echo "════════════════════════════════════════════════════════════"
for suite in "--lib" "--test forward_auth"; do
    printf '  %-24s ' "$suite"
    if output=$(cargo test $suite -- --test-threads=4 2>&1); then
        echo "$(echo "$output" | grep -E '^test result:' | head -1)"
    else
        echo "FAILED"
        echo "$output" | tail -20
        FAILED=1
    fi
done

unset TTYD_BIN TTYD_REFERENCE
run_suite_set "Rust implementation"

if [[ -n "$C_BINARY" ]]; then
    if [[ ! -x "$C_BINARY" ]]; then
        echo "error: $C_BINARY is not an executable" >&2
        exit 1
    fi
    export TTYD_BIN="$C_BINARY" TTYD_REFERENCE=1
    run_suite_set "C reference: $C_BINARY"
    unset TTYD_BIN TTYD_REFERENCE
fi

echo
if [[ $FAILED -eq 0 ]]; then
    echo "All suites passed."
else
    echo "Some suites failed; see above."
fi
exit $FAILED
