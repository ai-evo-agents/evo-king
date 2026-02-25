#!/usr/bin/env bash
# ─── evo-king pre-push checks ────────────────────────────────────────────────
#
# Run this before pushing to ensure CI will pass locally.
# Mirrors the exact checks in .github/workflows/ci.yml.
#
# Usage:
#   ./pre_push.sh           # run all checks
#   ./pre_push.sh --fix     # auto-fix formatting issues before checking
#
# Exit code: 0 = all passed, 1 = something failed

set -uo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
FIX=false

for arg in "$@"; do
    case "$arg" in
        --fix) FIX=true ;;
        --help|-h)
            echo "Usage: $0 [--fix]"
            echo "  --fix   Auto-fix cargo fmt issues before checking"
            exit 0
            ;;
    esac
done

# ── Colour helpers ────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[1;34m'; BOLD='\033[1m'; RESET='\033[0m'

step()  { echo -e "\n${BLUE}${BOLD}▶ $*${RESET}"; }
ok()    { echo -e "${GREEN}✓ $*${RESET}"; }
fail()  { echo -e "${RED}✗ $*${RESET}"; }
warn()  { echo -e "${YELLOW}⚠ $*${RESET}"; }

FAILED=()

run_check() {
    local label="$1"; shift
    step "$label"
    if "$@"; then
        ok "$label passed"
    else
        fail "$label FAILED"
        FAILED+=("$label")
    fi
}

cd "$REPO_DIR"

# ── 1. cargo fmt ─────────────────────────────────────────────────────────────
step "cargo fmt"
if $FIX; then
    if cargo fmt; then
        ok "cargo fmt (auto-fixed formatting)"
    else
        fail "cargo fmt failed"
        FAILED+=("cargo fmt")
    fi
else
    if cargo fmt --check; then
        ok "cargo fmt --check passed"
    else
        fail "cargo fmt --check FAILED (run with --fix to auto-format)"
        FAILED+=("cargo fmt --check")
    fi
fi

# ── 2. cargo clippy ───────────────────────────────────────────────────────────
run_check "cargo clippy" \
    cargo clippy --all-targets --all-features -- -D warnings

# ── 3. cargo build ───────────────────────────────────────────────────────────
run_check "cargo build" \
    cargo build

# ── 4. cargo test ────────────────────────────────────────────────────────────
run_check "cargo test" \
    cargo test

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}═══════════════════════════════${RESET}"
if [ ${#FAILED[@]} -eq 0 ]; then
    echo -e "${GREEN}${BOLD}✓ All checks passed — safe to push!${RESET}"
    exit 0
else
    echo -e "${RED}${BOLD}✗ ${#FAILED[@]} check(s) failed:${RESET}"
    for f in "${FAILED[@]}"; do
        echo -e "  ${RED}• $f${RESET}"
    done
    echo ""
    warn "Fix these before pushing to avoid CI failures."
    exit 1
fi
