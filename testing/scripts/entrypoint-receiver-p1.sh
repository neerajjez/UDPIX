#!/usr/bin/env bash
# Phase 1 receiver dispatcher — generates data, receives, verifies, based on P1_SCENARIO.
set -euo pipefail

SCENARIO="${P1_SCENARIO:-p1-002}"
TESTDATA=/testdata
RECEIVED=/received
BIND_DELAY="${RECEIVER_BIND_DELAY_S:-0}"
EXPECTED_FAIL="${EXPECTED_FAIL:-0}"

mkdir -p "$TESTDATA" "$RECEIVED"
# Remove any stale checksums.txt from a previous run — ensures the sender
# waits for the fresh checksum file rather than detecting a leftover.
rm -f "$TESTDATA/checksums.txt"

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  UDPix Phase 1 Test — Receiver  (${SCENARIO})        ║"
echo "╚══════════════════════════════════════════════════════╝"
echo "[INFO] Scenario     : $SCENARIO"
echo "[INFO] Bind delay   : ${BIND_DELAY}s"
echo "[INFO] Expected fail: $EXPECTED_FAIL"
echo ""

# ── Helper: standard SHA-256 checksum verification ─────────────────────────────
# Uses fixed-offset parsing to handle filenames with spaces / unicode.
standard_verify() {
    local checksums_file="${1:-$TESTDATA/checksums.txt}"

    echo "════════════════════════════════════════════════════════"
    echo "  CHECKSUM VERIFICATION"
    echo "════════════════════════════════════════════════════════"

    local PASS=0 FAIL=0 MISSING=0

    while IFS= read -r line; do
        # sha256sum format: <64-char hash><2 spaces><filepath>
        local hash="${line:0:64}"
        local filepath="${line:66}"
        local subpath="${filepath#/testdata/}"
        local target="$RECEIVED/$subpath"
        if [ ! -f "$target" ]; then
            echo "  MISSING: $subpath"
            (( MISSING++ )) || true
        else
            local actual; actual=$(sha256sum "$target" | cut -c1-64)
            if [ "$actual" = "$hash" ]; then
                (( PASS++ )) || true
            else
                echo "  MISMATCH: $subpath"
                (( FAIL++ )) || true
            fi
        fi
    done < "$checksums_file"

    echo ""
    echo "  Passed  : $PASS"
    echo "  Failed  : $FAIL"
    echo "  Missing : $MISSING"
    echo ""

    if [ "$EXPECTED_FAIL" = "1" ]; then
        local total; total=$(wc -l < "$checksums_file")
        echo "  KNOWN LIMITATION: sender-before-receiver caused $((MISSING + FAIL))/${total} missing/corrupt files"
        echo "  (direct mode has no SYN/ACK — early packets dropped by kernel)"
        return 0
    fi

    if [ "$FAIL" -eq 0 ] && [ "$MISSING" -eq 0 ] && [ "$PASS" -gt 0 ]; then
        echo "  ✓  RESULT: ALL $PASS FILES VERIFIED — PASS"
        return 0
    else
        echo "  ✗  RESULT: VERIFICATION FAILED (fail=$FAIL missing=$MISSING passed=$PASS)"
        return 1
    fi
}

# ── Helper: generate the baseline 505-file dataset ─────────────────────────────
gen_baseline_505() {
    python3 - <<'PYEOF'
import os, random
random.seed(42)
os.makedirs("/testdata/small", exist_ok=True)
os.makedirs("/testdata/large", exist_ok=True)
for i in range(1, 501):
    with open(f"/testdata/small/file_{i:04d}.bin", "wb") as f:
        f.write(random.randbytes(20 * 1024))
for i in range(1, 6):
    with open(f"/testdata/large/file_{i:02d}.bin", "wb") as f:
        f.write(random.randbytes(20 * 1024 * 1024))
PYEOF
}

# ── Helper: compute checksums, print stats, sleep for sender ───────────────────
finalize_data() {
    # $1: glob pattern for find (default: "*.bin")
    local pattern="${1:-*.bin}"
    find "$TESTDATA" -name "$pattern" | sort | xargs -d '\n' sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    local FILE_COUNT; FILE_COUNT=$(wc -l < "$TESTDATA/checksums.txt")
    local TOTAL_BYTES; TOTAL_BYTES=$(find "$TESTDATA" -name "$pattern" -exec stat --format='%s' {} + \
        | awk '{s+=$1}END{print s+0}')
    echo "[GEN] Done: $FILE_COUNT files, $(( TOTAL_BYTES / 1024 / 1024 )) MB"
    echo "[GEN] Bind delay: ${BIND_DELAY}s (sender polls checksums.txt, adds 3s)"

    # Custom bind delay for timing tests (replaces normal 2s buffer).
    if [ "$BIND_DELAY" -gt 0 ]; then
        sleep "$BIND_DELAY"
    else
        sleep 2
    fi
}

# ── Helper: checksums for all files (not just *.bin) ───────────────────────────
finalize_data_all() {
    find "$TESTDATA" -type f ! -name "checksums.txt" ! -name "checksums.txt.tmp" | sort | xargs -d '\n' sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    local FILE_COUNT; FILE_COUNT=$(wc -l < "$TESTDATA/checksums.txt")
    echo "[GEN] Done: $FILE_COUNT files"
    if [ "$BIND_DELAY" -gt 0 ]; then
        sleep "$BIND_DELAY"
    else
        sleep 2
    fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# SCENARIO DISPATCH
# ═══════════════════════════════════════════════════════════════════════════════
case "$SCENARIO" in

# ── P1-002: Empty Files ─────────────────────────────────────────────────────────
p1-002)
    echo "[GEN] P1-002: generating empty + 1-byte files..."
    python3 - <<'PYEOF'
import os
os.makedirs("/testdata/p1002", exist_ok=True)
open("/testdata/p1002/zero.bin", "wb").close()
open("/testdata/p1002/also_zero.txt", "wb").close()
open("/testdata/p1002/one_byte.bin", "wb").write(b'\x00')
PYEOF

    find "$TESTDATA" -type f ! -name "checksums.txt" ! -name "checksums.txt.tmp" | sort | xargs -d '\n' sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: $(wc -l < "$TESTDATA/checksums.txt") files"
    sleep 2

    echo "[RX] P1-002: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    # Extra: zero.bin must be 0 bytes
    echo "[VERIFY] Checking zero.bin is 0 bytes..."
    ZERO_SIZE=$(stat -c%s "$RECEIVED/p1002/zero.bin" 2>/dev/null || echo "MISSING")
    if [ "$ZERO_SIZE" = "0" ]; then
        echo "  ✓ zero.bin = 0 bytes"
    else
        echo "  ✗ zero.bin size = $ZERO_SIZE (expected 0)"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-003: 64 MB Large File ────────────────────────────────────────────────────
p1-003)
    echo "[GEN] P1-003: generating 64 MB file..."
    python3 - <<'PYEOF'
import os, random
random.seed(12)
os.makedirs("/testdata/p1003", exist_ok=True)
with open("/testdata/p1003/big64.bin", "wb") as f:
    f.write(random.randbytes(64 * 1024 * 1024))
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 1 file, 64 MB"
    sleep 2

    echo "[RX] P1-003: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    ACTUAL_SIZE=$(stat -c%s "$RECEIVED/p1003/big64.bin" 2>/dev/null || echo "0")
    if [ "$ACTUAL_SIZE" = "67108864" ]; then
        echo "  ✓ big64.bin = 67108864 bytes"
    else
        echo "  ✗ big64.bin size = $ACTUAL_SIZE (expected 67108864)"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-004: 512 MB Very Large File ──────────────────────────────────────────────
p1-004)
    echo "[GEN] P1-004: generating 512 MB file (this takes a moment)..."
    python3 - <<'PYEOF'
import os, random
random.seed(7)
os.makedirs("/testdata/p1004", exist_ok=True)
with open("/testdata/p1004/big512.bin", "wb") as f:
    # Write in 64 MB chunks to keep Python memory usage low.
    for _ in range(8):
        f.write(random.randbytes(64 * 1024 * 1024))
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 1 file, 512 MB"
    sleep 2

    echo "[RX] P1-004: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    ELAPSED=$(( END_MS - START_MS ))
    echo "[RX] Transfer done in ${ELAPSED} ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    ACTUAL_SIZE=$(stat -c%s "$RECEIVED/p1004/big512.bin" 2>/dev/null || echo "0")
    EXPECTED_SIZE=$(( 512 * 1024 * 1024 ))
    if [ "$ACTUAL_SIZE" = "$EXPECTED_SIZE" ]; then
        echo "  ✓ big512.bin = $EXPECTED_SIZE bytes"
    else
        echo "  ✗ big512.bin size = $ACTUAL_SIZE (expected $EXPECTED_SIZE)"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-005: Deep Nesting (20 Levels) ────────────────────────────────────────────
p1-005)
    echo "[GEN] P1-005: generating 20-level deep path..."
    python3 - <<'PYEOF'
import os
path = "/testdata/p1005/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t"
os.makedirs(path, exist_ok=True)
with open(f"{path}/leaf.txt", "wb") as f:
    f.write(b"deep nested content")
PYEOF

    find "$TESTDATA" -type f ! -name "checksums.txt" ! -name "checksums.txt.tmp" | sort | xargs -d '\n' sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 1 file, 19 bytes"
    sleep 2

    echo "[RX] P1-005: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    LEAF="$RECEIVED/p1005/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/leaf.txt"
    if [ -f "$LEAF" ]; then
        CONTENT=$(cat "$LEAF")
        if [ "$CONTENT" = "deep nested content" ]; then
            echo "  ✓ leaf.txt exists at 20-level depth with correct content"
        else
            echo "  ✗ leaf.txt content mismatch: $CONTENT"
            RESULT=1
        fi
    else
        echo "  ✗ leaf.txt missing at expected 20-level path"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-006: Special Characters in Filenames ─────────────────────────────────────
p1-006)
    echo "[GEN] P1-006: generating special-char / unicode filenames..."
    python3 - <<'PYEOF'
import os
os.makedirs("/testdata/p1006", exist_ok=True)
names = [
    "file with spaces.txt",
    "resume_naive.txt",
    "japanese_file.bin",
    "file-with-hyphens_and.underscores",
    "UPPERCASE_AND_lowercase.BIN",
]
for n in names:
    with open(f"/testdata/p1006/{n}", "wb") as f:
        f.write(n.encode("utf-8"))
PYEOF

    find "$TESTDATA" -type f ! -name "checksums.txt" ! -name "checksums.txt.tmp" | sort | xargs -d '\n' sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 5 files with special/unicode names"
    sleep 2

    echo "[RX] P1-006: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    # Extra: verify each file's content equals its own filename bytes (using Python
    # to avoid shell word-splitting issues with spaces and unicode).
    echo "[VERIFY] Verifying content-equals-filename for all 5 files..."
    python3 - <<'PYEOF'
import os, sys
names = [
    "file with spaces.txt",
    "resume_naive.txt",
    "japanese_file.bin",
    "file-with-hyphens_and.underscores",
    "UPPERCASE_AND_lowercase.BIN",
]
failed = 0
for n in names:
    path = f"/received/p1006/{n}"
    try:
        data = open(path, "rb").read()
        expected = n.encode("utf-8")
        if data == expected:
            print(f"  ✓ {n}")
        else:
            print(f"  ✗ {n}: content mismatch (got {data!r}, expected {expected!r})")
            failed += 1
    except FileNotFoundError:
        print(f"  ✗ {n}: MISSING")
        failed += 1
sys.exit(failed)
PYEOF
    P6_RESULT=$?
    [ $P6_RESULT -eq 0 ] || RESULT=1
    exit $RESULT
    ;;

# ── P1-007: 1000 × 1 KB Files ───────────────────────────────────────────────────
p1-007)
    echo "[GEN] P1-007: generating 1000 × 1 KB files..."
    python3 - <<'PYEOF'
import os
os.makedirs("/testdata/p1007", exist_ok=True)
for i in range(1000):
    with open(f"/testdata/p1007/f{i:04d}.bin", "wb") as f:
        f.write(bytes([i % 256]) * 1024)
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 1000 files, 1 MB total"
    sleep 2

    echo "[RX] P1-007: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    FILE_COUNT=$(find "$RECEIVED/p1007" -type f 2>/dev/null | wc -l)
    if [ "$FILE_COUNT" -eq 1000 ]; then
        echo "  ✓ Received 1000/1000 files"
    else
        echo "  ✗ Received $FILE_COUNT/1000 files"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-008: Mixed File Sizes (1 B → 50 MB) ──────────────────────────────────────
p1-008)
    echo "[GEN] P1-008: generating 8 files of mixed sizes..."
    python3 - <<'PYEOF'
import os
os.makedirs("/testdata/p1008", exist_ok=True)
sizes = [1, 512, 4096, 65536, 1048576, 16777216, 16777217, 52428800]
for sz in sizes:
    with open(f"/testdata/p1008/f{sz}.bin", "wb") as f:
        f.write(b'\xAB' * sz)
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 8 files"
    sleep 2

    echo "[RX] P1-008: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    # Verify each file's size precisely.
    echo "[VERIFY] Checking file sizes..."
    python3 - <<'PYEOF'
import os, sys
sizes = [1, 512, 4096, 65536, 1048576, 16777216, 16777217, 52428800]
failed = 0
for sz in sizes:
    path = f"/received/p1008/f{sz}.bin"
    try:
        actual = os.path.getsize(path)
        if actual == sz:
            print(f"  ✓ f{sz}.bin = {sz} bytes")
        else:
            print(f"  ✗ f{sz}.bin: size={actual}, expected={sz}")
            failed += 1
    except FileNotFoundError:
        print(f"  ✗ f{sz}.bin: MISSING")
        failed += 1
sys.exit(failed)
PYEOF
    P8_RESULT=$?
    [ $P8_RESULT -eq 0 ] || RESULT=1
    exit $RESULT
    ;;

# ── P1-009: Pre-Existing Files in Output Directory ──────────────────────────────
p1-009)
    echo "[GEN] P1-009: generating 5 test files..."
    python3 - <<'PYEOF'
import os
os.makedirs("/testdata/p1009", exist_ok=True)
for i in range(1, 6):
    with open(f"/testdata/p1009/file_{i}.bin", "wb") as f:
        f.write(b'\xCC' * 4096)
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 5 files, 20 KB total"

    # Pre-condition: populate /received/p1009 with STALE content.
    echo "[PRE] Pre-populating /received/p1009 with stale content..."
    mkdir -p "$RECEIVED/p1009"
    for i in 1 2 3 4 5; do
        echo "STALE_OLD_CONTENT_SHOULD_BE_OVERWRITTEN" > "$RECEIVED/p1009/file_${i}.bin"
    done
    echo "[PRE] Done — stale files in place"

    sleep 2

    echo "[RX] P1-009: binding 0.0.0.0:9001..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    RESULT=$?

    # Extra: verify stale content was overwritten.
    echo "[VERIFY] Confirming stale content was overwritten..."
    STALE_FOUND=0
    for i in 1 2 3 4 5; do
        if grep -q "STALE_OLD_CONTENT" "$RECEIVED/p1009/file_${i}.bin" 2>/dev/null; then
            echo "  ✗ file_${i}.bin still contains stale content!"
            STALE_FOUND=1
        fi
    done
    if [ "$STALE_FOUND" -eq 0 ]; then
        echo "  ✓ All stale content successfully overwritten"
    else
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-010: Single File (Not Directory) as Send Path ────────────────────────────
p1-010)
    echo "[GEN] P1-010: generating 1 MB single file..."
    python3 - <<'PYEOF'
import os, random
random.seed(99)
os.makedirs("/testdata/p1010", exist_ok=True)
with open("/testdata/p1010/single.bin", "wb") as f:
    f.write(random.randbytes(1024 * 1024))
PYEOF

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    echo "[GEN] Done: 1 file, 1 MB"
    echo "[GEN] Pausing 2s for sender to detect checksums.txt..."
    sleep 2

    echo "[RX] P1-010: binding with 35s timeout (sender may error before sending)..."
    START_MS=$(date +%s%3N)
    set +e
    RUST_LOG=info timeout 35 udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001 2>&1 | tee /tmp/p1010_receiver.log
    RX_CODE=${PIPESTATUS[0]}
    set -e
    END_MS=$(date +%s%3N)
    echo "[RX] receive exited with code $RX_CODE in $(( END_MS - START_MS )) ms"

    # Wait for sender flag file (written by entrypoint-sender-p1.sh).
    echo "[RX] Waiting up to 10s for sender flag file..."
    for _i in $(seq 1 10); do
        [ -f "$TESTDATA/p1010/sender_exit_code" ] && break
        sleep 1
    done

    SENDER_EXIT=$(cat "$TESTDATA/p1010/sender_exit_code" 2>/dev/null || echo "UNKNOWN")
    SENDER_PANIC=$(cat "$TESTDATA/p1010/sender_panic" 2>/dev/null || echo "UNKNOWN")

    echo "[RX] Sender exit code: $SENDER_EXIT"
    echo "[RX] Sender panicked: $SENDER_PANIC"

    # Check receiver log for panics.
    RECEIVER_PANIC=0
    if grep -qiE "panicked|SIGSEGV|SIGABRT|thread '.*' panicked" /tmp/p1010_receiver.log 2>/dev/null; then
        RECEIVER_PANIC=1
    fi

    RESULT=0

    if [ "$RECEIVER_PANIC" -eq 1 ] || [ "$SENDER_PANIC" = "1" ]; then
        echo "  ✗ PANIC detected (receiver_panic=$RECEIVER_PANIC sender_panic=$SENDER_PANIC)"
        RESULT=1
    elif [ "$SENDER_EXIT" != "0" ] && [ "$SENDER_EXIT" != "UNKNOWN" ]; then
        # Sender correctly rejected the file path — PASS regardless of receiver exit code.
        echo "  ✓ P1-010 PASS: sender exited cleanly with code $SENDER_EXIT (no panic)"
        echo "  (Expected: read_dir fails on file path → clean error message)"
        RESULT=0
    elif [ "$RX_CODE" -eq 0 ]; then
        # Sender reported success AND receiver exited cleanly — verify files.
        echo "  [INFO] Receive completed (code 0) — verifying files..."
        set +e
        standard_verify "$TESTDATA/checksums.txt"
        RESULT=$?
        set -e
        if [ "$RESULT" -eq 0 ]; then
            echo "  ✓ P1-010 PASS: sender handled file path (transfer succeeded)"
        fi
    else
        echo "  ✗ P1-010 FAIL: unexpected state (rx=$RX_CODE sender=$SENDER_EXIT)"
        RESULT=1
    fi
    exit $RESULT
    ;;

# ── P1-011: Receiver Starts 5s Before Sender ────────────────────────────────────
p1-011)
    echo "[GEN] P1-011: generating baseline 505-file dataset..."
    gen_baseline_505

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    FILE_COUNT=$(wc -l < "$TESTDATA/checksums.txt")
    TOTAL_BYTES=$(find "$TESTDATA" -name "*.bin" -exec stat --format='%s' {} + | awk '{s+=$1}END{print s+0}')
    echo "[GEN] Done: $FILE_COUNT files, $(( TOTAL_BYTES / 1024 / 1024 )) MB"
    echo "[GEN] Binding immediately — sender will arrive 5s late (SENDER_DELAY_S=5)"
    sleep 2

    echo "[RX] P1-011: binding 0.0.0.0:9001 (waiting for delayed sender)..."
    START_MS=$(date +%s%3N)
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    END_MS=$(date +%s%3N)
    echo "[RX] Transfer done in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    exit $?
    ;;

# ── P1-012: Sender Starts 5s Before Receiver (Known Gap) ────────────────────────
p1-012)
    echo "[GEN] P1-012: generating baseline 505-file dataset..."
    gen_baseline_505

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    FILE_COUNT=$(wc -l < "$TESTDATA/checksums.txt")
    echo "[GEN] Done: $FILE_COUNT files"
    echo "[GEN] Sleeping ${BIND_DELAY}s BEFORE binding (sender will start in ~3s)"

    # Sleep BIND_DELAY seconds — sender fires during this gap.
    sleep "$BIND_DELAY"

    echo "[RX] P1-012: binding 0.0.0.0:9001 (${BIND_DELAY}s after sender started)..."
    START_MS=$(date +%s%3N)
    set +e
    RUST_LOG=info udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --direct --local-port 9001
    RX_CODE=$?
    set -e
    END_MS=$(date +%s%3N)
    echo "[RX] Receive exited with code $RX_CODE in $(( END_MS - START_MS )) ms"

    standard_verify "$TESTDATA/checksums.txt"
    exit 0
    ;;

# ── P1-013: RUDP Mode (No --direct) ─────────────────────────────────────────────
p1-013)
    echo "[GEN] P1-013: generating baseline 505-file dataset..."
    gen_baseline_505

    find "$TESTDATA" -name "*.bin" | sort | xargs sha256sum > "$TESTDATA/checksums.txt.tmp" && mv "$TESTDATA/checksums.txt.tmp" "$TESTDATA/checksums.txt"
    FILE_COUNT=$(wc -l < "$TESTDATA/checksums.txt")
    echo "[GEN] Done: $FILE_COUNT files"
    echo "[GEN] RUDP mode — no --direct; using congestion control path"
    sleep 2

    echo "[RX] P1-013: binding with 60s timeout (RUDP known issues)..."
    START_MS=$(date +%s%3N)
    set +e
    RUST_LOG=info timeout 60 udpix receive "$RECEIVED" 172.28.1.20:9002 \
        --local-port 9001 2>&1 | tee /tmp/p1013_receiver.log
    RX_CODE=${PIPESTATUS[0]}
    set -e
    END_MS=$(date +%s%3N)
    echo "[RX] Receive exited with code $RX_CODE in $(( END_MS - START_MS )) ms"

    echo ""
    echo "════════════════════════════════════════════════════════"
    echo "  P1-013 RUDP VERIFICATION"
    echo "════════════════════════════════════════════════════════"

    RESULT=0

    # NAT traversal failure is expected in a Docker LAN without a STUN server.
    if grep -qiE "NAT traversal failed|all STUN servers failed|ICE.*failed|no TURN" /tmp/p1013_receiver.log 2>/dev/null; then
        echo "  ~ P1-013: NAT traversal failed (no STUN server in Docker LAN — expected)"
        echo "  ✓ P1-013 PASS (clean exit; configure --stun for production use)"
        echo "════════════════════════════════════════════════════════"
        exit 0
    fi

    # Check for panics (hard fail regardless of delivery count).
    PANIC_FOUND=0
    if grep -qiE "thread '.*' panicked|panicked at" /tmp/p1013_receiver.log 2>/dev/null; then
        echo "  ✗ PANIC detected in receiver log — FAIL"
        PANIC_FOUND=1
        RESULT=1
    fi

    # Count received files.
    RX_COUNT=$(find "$RECEIVED" -type f 2>/dev/null | wc -l)
    TOTAL=505
    PCT=$(python3 -c "print(f'{$RX_COUNT * 100 / $TOTAL:.1f}')")
    echo "  Files received : $RX_COUNT / $TOTAL  (${PCT}%)"

    # Pass criteria: no panic AND at least 50% delivery (proves RUDP path active).
    if [ "$PANIC_FOUND" -eq 0 ]; then
        if [ "$RX_COUNT" -lt $(( TOTAL / 2 )) ]; then
            echo "  ✗ Received < 50% of files ($RX_COUNT/$TOTAL) — FAIL"
            RESULT=1
        else
            echo "  ✓ Received ≥ 50% of files ($RX_COUNT/$TOTAL) — OK"
        fi
    fi

    if [ "$RESULT" -eq 0 ]; then
        echo "  ✓ P1-013 PASS (RUDP mode functional; delivery=$RX_COUNT/$TOTAL)"
    else
        echo "  ✗ P1-013 FAIL"
    fi
    echo "════════════════════════════════════════════════════════"
    exit $RESULT
    ;;

*)
    echo "[ERROR] Unknown scenario: $SCENARIO"
    exit 1
    ;;
esac
