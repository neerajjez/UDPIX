#!/usr/bin/env bash
# Phase 1 sender dispatcher — waits for receiver, then sends based on P1_SCENARIO.
set -euo pipefail

SCENARIO="${P1_SCENARIO:-p1-002}"
TESTDATA=/testdata
EXTRA_DELAY="${SENDER_DELAY_S:-0}"

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  UDPix Phase 1 Test — Sender  (${SCENARIO})          ║"
echo "╚══════════════════════════════════════════════════════╝"
echo "[TX] Scenario    : $SCENARIO"
echo "[TX] Extra delay : ${EXTRA_DELAY}s"
echo ""

# Wait for receiver to finish generating test data.
echo "[TX] Waiting for checksums.txt..."
until [ -f "$TESTDATA/checksums.txt" ]; do sleep 1; done

# Base buffer: let the receiver bind its UDP socket.
sleep 3

# Optional extra delay (P1-011: sender starts 5s after receiver binds).
if [ "$EXTRA_DELAY" -gt 0 ]; then
    echo "[TX] Extra delay ${EXTRA_DELAY}s (P1-011 timing test)..."
    sleep "$EXTRA_DELAY"
fi

START_MS=$(date +%s%3N)

case "$SCENARIO" in

    p1-002|p1-003|p1-004|p1-005|p1-007|p1-008|p1-009|p1-011|p1-012)
        # Standard direct-mode send of the whole /testdata directory.
        RUST_LOG=info udpix send "$TESTDATA" 172.28.1.10:9001 \
            --direct \
            --local-port 9002
        EXIT_CODE=$?
        ;;

    p1-006)
        # Special characters / unicode filenames — still just send /testdata.
        RUST_LOG=info udpix send "$TESTDATA" 172.28.1.10:9001 \
            --direct \
            --local-port 9002
        EXIT_CODE=$?
        ;;

    p1-010)
        # Send a FILE path (not directory) — expected to error cleanly.
        set +e
        RUST_LOG=error udpix send "$TESTDATA/p1010/single.bin" 172.28.1.10:9001 \
            --direct \
            --local-port 9002 2>&1 | tee /tmp/p1010_sender.log
        EXIT_CODE=${PIPESTATUS[0]}
        set -e

        # Signal receiver: write exit code and panic flag to shared testdata volume.
        echo "$EXIT_CODE" > "$TESTDATA/p1010/sender_exit_code"
        if grep -q "panicked\|SIGSEGV\|SIGABRT" /tmp/p1010_sender.log 2>/dev/null; then
            echo "1" > "$TESTDATA/p1010/sender_panic"
        else
            echo "0" > "$TESTDATA/p1010/sender_panic"
        fi
        echo "[TX] P1-010: sender exited with code $EXIT_CODE"
        # Send a FIN-like signal: receiver is waiting; we must unblock it.
        # Since our send errored, no FIN was sent. Receiver has a timeout.
        # Sleep to let receiver's timeout fire, then exit.
        sleep 40
        exit 0
        ;;

    p1-013)
        # RUDP mode — no --direct flag; use congestion control path.
        set +e
        RUST_LOG=info udpix send "$TESTDATA" 172.28.1.10:9001 \
            --local-port 9002 2>&1 | tee /tmp/p1013_sender.log
        EXIT_CODE=${PIPESTATUS[0]}
        set -e
        ;;

    *)
        echo "[TX] Unknown scenario: $SCENARIO — falling back to direct send."
        RUST_LOG=info udpix send "$TESTDATA" 172.28.1.10:9001 \
            --direct \
            --local-port 9002
        EXIT_CODE=$?
        ;;
esac

END_MS=$(date +%s%3N)
ELAPSED=$(( END_MS - START_MS ))

echo ""
echo "════════════════════════════════════════════════════════"
echo "  SENDER COMPLETE  ($SCENARIO)"
echo "════════════════════════════════════════════════════════"
echo "  Exit code : $EXIT_CODE"
echo "  Duration  : ${ELAPSED} ms"
echo "════════════════════════════════════════════════════════"
echo ""

# Stay alive while receiver verifies checksums.
echo "[TX] Waiting for receiver to verify and exit..."
sleep 120
