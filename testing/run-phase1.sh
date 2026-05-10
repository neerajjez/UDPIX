#!/usr/bin/env bash
# Run all 13 Phase 1 functional-correctness tests sequentially.
# Usage: bash testing/run-phase1.sh [--rebuild]
#
# Each test spins up two Docker containers (sender + receiver) on a 172.28.1.0/24
# bridge network, transfers a scenario-specific dataset, verifies checksums, and
# reports PASS / FAIL / KNOWN-FAIL.
#
# Test results are printed as a summary table at the end.
# Exit code: 0 if all non-expected-fail tests pass; 1 otherwise.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
COMPOSE_BASELINE="$SCRIPT_DIR/docker-compose.yml"
COMPOSE_PHASE1="$SCRIPT_DIR/docker-compose.phase1.yml"

REBUILD=0
if [[ "${1:-}" == "--rebuild" ]]; then
    REBUILD=1
fi

# ── Scenario table: id:sender_delay:receiver_delay:expected_fail ───────────────
SCENARIOS=(
    "p1-001:0:0:0"
    "p1-002:0:0:0"
    "p1-003:0:0:0"
    "p1-004:0:0:0"
    "p1-005:0:0:0"
    "p1-006:0:0:0"
    "p1-007:0:0:0"
    "p1-008:0:0:0"
    "p1-009:0:0:0"
    "p1-010:0:0:0"
    "p1-011:5:0:0"
    "p1-012:0:5:1"
    "p1-013:0:0:0"
)

# Result storage: indexed by scenario id
declare -A RESULT_STATUS
declare -A RESULT_TIME

# ── Build images once ──────────────────────────────────────────────────────────
echo ""
echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║  UDPix — Phase 1 Functional Correctness Tests                   ║"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""
echo "[BUILD] Building Docker test image..."
if [ "$REBUILD" -eq 1 ]; then
    docker compose -f "$COMPOSE_PHASE1" build --no-cache
else
    docker compose -f "$COMPOSE_PHASE1" build
fi
echo "[BUILD] Done."
echo ""

# ── Run each scenario ──────────────────────────────────────────────────────────
for entry in "${SCENARIOS[@]}"; do
    IFS=':' read -r SCENARIO SENDER_DELAY RECEIVER_DELAY EXPECTED_FAIL <<< "$entry"

    echo "────────────────────────────────────────────────────────────────────"
    echo "  Running: $SCENARIO  (sender_delay=${SENDER_DELAY}s  rx_delay=${RECEIVER_DELAY}s  expected_fail=${EXPECTED_FAIL})"
    echo "────────────────────────────────────────────────────────────────────"

    T_START=$(date +%s)

    if [ "$SCENARIO" = "p1-001" ]; then
        # P1-001 uses the original compose file + original entrypoint scripts.
        set +e
        docker compose -f "$COMPOSE_BASELINE" up \
            --abort-on-container-exit \
            --exit-code-from receiver \
            2>&1
        COMPOSE_EXIT=$?
        set -e
        docker compose -f "$COMPOSE_BASELINE" down --volumes 2>/dev/null || true
    else
        set +e
        P1_SCENARIO="$SCENARIO" \
        SENDER_DELAY_S="$SENDER_DELAY" \
        RECEIVER_BIND_DELAY_S="$RECEIVER_DELAY" \
        EXPECTED_FAIL="$EXPECTED_FAIL" \
        docker compose -f "$COMPOSE_PHASE1" up \
            --abort-on-container-exit \
            --exit-code-from receiver \
            2>&1
        COMPOSE_EXIT=$?
        set -e
        P1_SCENARIO="$SCENARIO" \
        docker compose -f "$COMPOSE_PHASE1" down --volumes 2>/dev/null || true
    fi

    T_END=$(date +%s)
    ELAPSED=$(( T_END - T_START ))

    if [ "$EXPECTED_FAIL" -eq 1 ]; then
        STATUS="KNOWN-FAIL"
    elif [ "$COMPOSE_EXIT" -eq 0 ]; then
        STATUS="PASS"
    else
        STATUS="FAIL"
    fi

    RESULT_STATUS["$SCENARIO"]="$STATUS"
    RESULT_TIME["$SCENARIO"]="${ELAPSED}s"

    echo ""
    echo "  $SCENARIO → $STATUS  (${ELAPSED}s)"
    echo ""
done

# ── Summary table ──────────────────────────────────────────────────────────────
echo ""
echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║  Phase 1 Test Summary                                           ║"
echo "╠══════════════════════════════════════════════════════════════════╣"
printf "║  %-10s  %-12s  %-8s  %-28s ║\n" "SCENARIO" "STATUS" "TIME" "DESCRIPTION"
echo "╠══════════════════════════════════════════════════════════════════╣"

declare -A DESCRIPTIONS=(
    ["p1-001"]="Baseline 505-file transfer"
    ["p1-002"]="Empty + 1-byte files"
    ["p1-003"]="64 MB large file"
    ["p1-004"]="512 MB very large file"
    ["p1-005"]="20-level deep path"
    ["p1-006"]="Special-char filenames"
    ["p1-007"]="1000 x 1KB files"
    ["p1-008"]="Mixed sizes (1B to 50MB)"
    ["p1-009"]="Pre-existing file overwrite"
    ["p1-010"]="Single file (not dir) path"
    ["p1-011"]="Receiver 5s before sender"
    ["p1-012"]="Sender 5s before receiver"
    ["p1-013"]="RUDP mode (no --direct)"
)

TOTAL_PASS=0
TOTAL_FAIL=0
TOTAL_KNOWN=0

for entry in "${SCENARIOS[@]}"; do
    IFS=':' read -r SCENARIO _ _ _ <<< "$entry"
    STATUS="${RESULT_STATUS[$SCENARIO]:-UNKNOWN}"
    TIME="${RESULT_TIME[$SCENARIO]:-?}"
    DESC="${DESCRIPTIONS[$SCENARIO]:-}"

    # Colour the status field.
    case "$STATUS" in
        PASS)       MARK="✓  PASS      " ; (( TOTAL_PASS++ )) || true ;;
        FAIL)       MARK="✗  FAIL      " ; (( TOTAL_FAIL++ )) || true ;;
        KNOWN-FAIL) MARK="~  KNOWN-FAIL" ; (( TOTAL_KNOWN++ )) || true ;;
        *)          MARK="?  UNKNOWN   " ; (( TOTAL_FAIL++ )) || true ;;
    esac

    printf "║  %-10s  %-12s  %-8s  %-28s ║\n" "$SCENARIO" "$MARK" "$TIME" "$DESC"
done

echo "╠══════════════════════════════════════════════════════════════════╣"
printf "║  %-63s ║\n" "Passed: $TOTAL_PASS  |  Failed: $TOTAL_FAIL  |  Known-fail: $TOTAL_KNOWN"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""

if [ "$TOTAL_FAIL" -eq 0 ]; then
    echo "All tests passed (or are known-fail). Phase 1 complete."
    exit 0
else
    echo "$TOTAL_FAIL test(s) FAILED. See logs above."
    exit 1
fi
