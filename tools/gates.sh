#!/usr/bin/env bash
# Quality gates from .claude/rules/workflow.md ("Bitti demeden önce" 1-4).
# One line per gate; output is shown only for a red gate. Exit 1 if any is red.
#
# Usage: tools/gates.sh            all gates
#        tools/gates.sh fmt clippy  only the named gates (fmt clippy test hal)
#
# `cargo test` runs with --no-fail-fast: without it cargo stops at the first
# failing test binary and the pass/fail totals are incomplete.
set -u
cd "$(dirname "$0")/.." || exit 2

red=0
log=$(mktemp) || exit 2
trap 'rm -f "$log"' EXIT

test_totals() {
    awk '/^test result:/ { p += $4; f += $6; n++ }
         END { printf " (suites=%d passed=%d failed=%d)", n, p, f }' "$log"
}

# gate <name> <filter-regex> <command...>
gate() {
    local name=$1 filter=$2
    shift 2
    local start=$SECONDS
    if "$@" >"$log" 2>&1; then
        local extra=""
        [ "$name" = test ] && extra=$(test_totals)
        printf 'OK   %-7s %3ss%s\n' "$name" $((SECONDS - start)) "$extra"
    else
        red=1
        local extra=""
        [ "$name" = test ] && extra=$(test_totals)
        printf 'FAIL %-7s %3ss%s\n' "$name" $((SECONDS - start)) "$extra"
        if grep -qE "$filter" "$log"; then
            grep -E "$filter" "$log" | head -n 40 | sed 's/^/     /'
        else
            tail -n 30 "$log" | sed 's/^/     /'
        fi
    fi
}

want() { [ $# -eq 0 ] && return 0; local g; for g in "${selected[@]}"; do [ "$g" = "$1" ] && return 0; done; return 1; }
selected=("$@")
sel() { [ ${#selected[@]} -eq 0 ] || want "$1"; }

sel fmt && gate fmt '^Diff in' \
    cargo fmt --all -- --check
sel clippy && gate clippy '^(error|warning)|^ +--> ' \
    cargo clippy --workspace -- -D warnings
sel test && gate test '^test .* FAILED|panicked at|^---- |^error' \
    cargo test --workspace --no-fail-fast
sel hal && gate hal '^(error|warning)|^ +--> ' \
    cargo check -p sandstar-server --no-default-features --features linux-hal,svm

exit $red
