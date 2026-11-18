#!/usr/bin/env bash
# Quality gates from .claude/rules/workflow.md ("Bitti demeden önce" 1-4).
# One line per gate; output is shown only for a red gate. Exit 1 if any is red.
#
# Usage: tools/gates.sh            all gates
#        tools/gates.sh fmt clippy  only the named gates (fmt clippy test hal docs)
#
# `docs` is a reminder, not a gate: it never makes the exit code 1.
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

# docs: reminder only, never red (workflow.md "Doküman disiplini").
# Compares the working tree with the merge-base of master and lists documents
# from tools/docs-map.tsv whose code paths changed but which did not change.
docs_reminders() {
    local map=tools/docs-map.tsv base
    base=$(git merge-base HEAD origin/master 2>/dev/null || git merge-base HEAD master 2>/dev/null) || {
        echo "SKIP docs    (no merge-base with master)"; return; }
    local changed untracked
    changed=$(git diff --name-only "$base"; git ls-files --others --exclude-standard)
    untracked=$(git ls-files --others --exclude-standard)
    [ -n "$changed" ] || { echo "OK   docs    (no changes against master)"; return; }

    # Added/removed lines of a tracked file, or the whole content of a new file.
    file_changes() {
        if printf '%s\n' "$untracked" | grep -qxF -- "$1"; then
            cat -- "$1" 2>/dev/null
        else
            git diff "$base" -- "$1" 2>/dev/null | grep -E '^[+-]' | grep -vE '^(\+\+\+|---) '
        fi
    }

    local out="" pat content doc reason files f hits
    while IFS=$'\t' read -r pat content doc reason; do
        case $pat in ''|'#'*) continue ;; esac
        files=$(printf '%s\n' "$changed" | grep -E -- "$pat") || continue
        printf '%s\n' "$changed" | grep -qxF -- "$doc" && continue
        hits=()
        while IFS= read -r f; do
            if [ "$content" = - ] || file_changes "$f" | grep -qE -- "$content"; then
                hits+=("$f")
            fi
        done <<<"$files"
        [ ${#hits[@]} -eq 0 ] && continue
        local more=""
        [ ${#hits[@]} -gt 1 ] && more=" +$((${#hits[@]} - 1))"
        out+="$doc  ($reason; ${hits[0]}$more)"$'\n'
    done <"$map"

    if [ -z "$out" ]; then
        echo "OK   docs    (no reminders)"
    else
        out=$(printf '%s' "$out" | sort -u)
        printf 'NOTE docs    %s document(s) to check (not a failure)\n' "$(printf '%s\n' "$out" | wc -l)"
        printf '%s\n' "$out" | sed 's/^/     /'
    fi
}
sel docs && docs_reminders

exit $red
