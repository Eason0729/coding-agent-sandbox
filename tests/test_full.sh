#!/usr/bin/env bash
# test_full.sh — End-to-end integration tests for CAS
# Run from project root: bash test_full.sh
# Uses ./tmp as working directory for all test state.

set -euo pipefail

BINARY="$(realpath ./target/debug/cas)"
TMP="$(realpath .)/tmp/full_test"
PROJECT="$TMP/project"

# ── helpers ───────────────────────────────────────────────────────────────────

PASS=0
FAIL=0

pass() {
	echo "[PASS] $1"
	PASS=$((PASS + 1))
}
fail() {
	echo "[FAIL] $1"
	FAIL=$((FAIL + 1))
}

run_in() {
	# run_in <project_dir> <cmd...>
	local proj="$1"
	shift
	(cd "$proj" && "$BINARY" run "$@" 2>&1)
}

# ── setup ─────────────────────────────────────────────────────────────────────

echo "=== Building ==="
cargo build 2>&1 | grep -E '^error' && exit 1 || true
echo "Build OK"
echo ""

rm -rf "$TMP"
mkdir -p "$PROJECT"

# ── Test 1: cas init ─────────────────────────────────────────────────────────
echo "=== Test 1: cas init ==="
(cd "$PROJECT" && "$BINARY" init)

if [ -d "$PROJECT/.sandbox" ] && [ -f "$PROJECT/.sandbox/config.toml" ]; then
	pass "cas init creates .sandbox and config.toml"
else
	fail "cas init missing expected files"
fi

# Double init should fail
set +e
INIT2_OUT=$( (cd "$PROJECT" && "$BINARY" init) 2>&1)
INIT2_CODE=$?
set -e
if [ "$INIT2_CODE" != "0" ] && echo "$INIT2_OUT" | grep -q "already exists"; then
	pass "cas init fails gracefully if already initialized"
else
	fail "cas init should fail when already initialized (code=$INIT2_CODE out=$INIT2_OUT)"
fi

# ── Test 2: Basic command execution ───────────────────────────────────────────
echo ""
echo "=== Test 2: Basic command execution ==="

OUT=$(run_in "$PROJECT" echo "hello_from_sandbox")
if echo "$OUT" | grep -q "hello_from_sandbox"; then
	pass "basic echo command runs inside sandbox"
else
	fail "basic echo failed — got: $OUT"
fi

# ── Test 3: Exit code propagation ─────────────────────────────────────────────
echo ""
echo "=== Test 3: Exit code propagation ==="

set +e
(cd "$PROJECT" && "$BINARY" run bash -c "exit 42" 2>/dev/null)
CODE=$?
set -e

if [ "$CODE" = "42" ]; then
	pass "exit code 42 propagated"
else
	fail "exit code should be 42, got $CODE"
fi

# ── Test 4: /proc visible ─────────────────────────────────────────────────────
echo ""
echo "=== Test 4: /proc visible inside sandbox ==="

set +e
OUT=$(run_in "$PROJECT" test -f /proc/1/status 2>&1)
CODE=$?
set -e

if [ "$CODE" = "0" ]; then
	pass "/proc/1/status visible inside sandbox"
else
	fail "/proc not accessible — got: $OUT"
fi

# ── Test 5: /dev/null usable ──────────────────────────────────────────────────
echo ""
echo "=== Test 5: /dev/null usable ==="

set +e
OUT=$(run_in "$PROJECT" bash -c "echo discarded > /dev/null && echo ok")
set -e

if echo "$OUT" | grep -q "ok"; then
	pass "/dev/null writable inside sandbox"
else
	fail "/dev/null unusable — got: $OUT"
fi

# ── Test 6: CoW — external file not modified ──────────────────────────────────
echo ""
echo "=== Test 6: CoW write does not modify external real file ==="

# Files inside the project root are Passthrough. CoW applies to files outside
# the project root (e.g., other directories on the host).
COW_TESTFILE="$TMP/cow_external.txt"
echo "original" >"$COW_TESTFILE"

run_in "$PROJECT" bash -c "echo 'modified' > '$COW_TESTFILE'" >/dev/null 2>&1 || true

REAL=$(cat "$COW_TESTFILE")
if [ "$REAL" = "original" ]; then
	pass "CoW write does not touch external real file"
else
	fail "CoW write modified external real file — got: $REAL"
fi

# ── Test 7: Read host FS through FUSE ─────────────────────────────────────────
echo ""
echo "=== Test 7: Read host filesystem through FUSE ==="

set +e
OUT=$(run_in "$PROJECT" ls /bin 2>&1)
CODE=$?
set -e

if [ "$CODE" = "0" ] && [ -n "$OUT" ]; then
	pass "/bin visible inside sandbox through FUSE"
else
	fail "/bin not accessible — got: $OUT"
fi

# ── Test 8: .sandbox hidden ───────────────────────────────────────────────────
echo ""
echo "=== Test 8: .sandbox hidden inside sandbox ==="

set +e
OUT=$(run_in "$PROJECT" bash -c "ls '$PROJECT/.sandbox' 2>&1 | head -1")
set -e

# Should either fail (no such file) or list empty content
if echo "$OUT" | grep -qE "(No such file|cannot access|ls:)" || [ -z "$OUT" ]; then
	pass ".sandbox not visible inside sandbox"
else
	fail ".sandbox should be hidden, but got: $OUT"
fi

# ── Test 9: Multiple concurrent cas run ───────────────────────────────────────
echo ""
echo "=== Test 9: Multiple concurrent sessions share one daemon ==="

PROJ2="$TMP/proj2"
cp -r "$PROJECT" "$PROJ2"

# Start two sessions slightly offset; each should echo its ID
OUT1_FILE="$TMP/out1.txt"
OUT2_FILE="$TMP/out2.txt"
(run_in "$PROJ2" bash -c "sleep 0.2 && echo session1" >"$OUT1_FILE" 2>&1) &
PID1=$!
sleep 0.05
run_in "$PROJ2" bash -c "echo session2" >"$OUT2_FILE" 2>&1 || true
wait $PID1 || true
OUT1=$(cat "$OUT1_FILE" 2>/dev/null || true)
OUT2=$(cat "$OUT2_FILE" 2>/dev/null || true)

if echo "$OUT1" | grep -q "session1" && echo "$OUT2" | grep -q "session2"; then
	pass "two concurrent sandboxes both complete"
else
	fail "concurrent sessions failed: OUT1='$OUT1' OUT2='$OUT2'"
fi

# ── Test 10: interactive shell (no SIGSYS) ────────────────────────────────────
echo ""
echo "=== Test 10: interactive bash shell ==="
# Regression test for epoll_create1 / setfsuid / setfsgid being blocked.
# With no controlling terminal, bash -i exits 0 if all syscalls are allowed,
# but is killed with SIGSYS if any required syscall is missing from the filter.
# We also test with a real PTY via python3 to exercise the readline (epoll) path.

# Sub-test A: stdin=/dev/null, forced interactive (-i)
set +e
OUT_IA=$(run_in "$PROJECT" bash -i </dev/null 2>&1)
CODE_IA=$?
set -e

if [ "$CODE_IA" = "0" ]; then
	pass "interactive bash (-i < /dev/null) exits 0 (no SIGSYS)"
else
	fail "interactive bash (-i < /dev/null) failed with code=$CODE_IA out=$OUT_IA"
fi

# Sub-test B: real PTY via python3 — exercises epoll_create1 (readline)
set +e
PTY_OUT=$(
	python3 - <<'PYEOF' 2>&1
import pty, os, sys, time, select, fcntl, termios
master, slave = pty.openpty()
pid = os.fork()
if pid == 0:
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    os.dup2(slave, 0); os.dup2(slave, 1); os.dup2(slave, 2)
    os.close(master)
    if slave > 2: os.close(slave)
    os.execlp("./target/debug/cas", "./target/debug/cas", "run", "bash")
else:
    os.close(slave)
    time.sleep(2)
    for cmd in [b"echo pty_shell_ok\n", b"exit\n"]:
        os.write(master, cmd)
        time.sleep(0.5)
    time.sleep(1)
    buf = b""
    while True:
        r, _, _ = select.select([master], [], [], 1)
        if not r: break
        try: buf += os.read(master, 4096)
        except: break
    text = buf.decode(errors='replace')
    if 'pty_shell_ok' in text and 'not implemented' not in text:
        print("OK")
    else:
        print("FAIL:" + repr(text[:300]))
    _, status = os.waitpid(pid, os.WNOHANG)
    sys.exit(0)
PYEOF
)
PTY_CODE=$?
set -e

if echo "$PTY_OUT" | grep -q "^OK"; then
	pass "interactive bash with real PTY works (epoll_create1 allowed)"
else
	fail "interactive bash PTY test failed: $PTY_OUT"
fi

# Sub-test C: openpty inside sandbox should work for TUI apps (e.g. opencode)
set +e
OPENPTY_OUT=$(
	run_in "$PROJECT" python3 - <<'PYEOF'
import pty
pty.openpty()
print("OPENPTY_OK")
PYEOF
)
OPENPTY_CODE=$?
set -e

if [ "$OPENPTY_CODE" = "0" ] && echo "$OPENPTY_OUT" | grep -q "OPENPTY_OK"; then
	pass "pty.openpty works inside sandbox"
else
	fail "pty.openpty failed in sandbox (code=$OPENPTY_CODE out=$OPENPTY_OUT)"
fi

# ── Test 11: cas clean ────────────────────────────────────────────────────────
echo ""
echo "=== Test 11: cas clean ==="

(cd "$PROJECT" && "$BINARY" clean 2>/dev/null || true)

if [ ! -d "$PROJECT/.sandbox/data" ]; then
	pass "cas clean removes .sandbox/data"
else
	fail "cas clean should have removed .sandbox/data"
fi

# ── Test 12: cas run after clean requires re-init ─────────────────────────────
echo ""
echo "=== Test 12: cas run after clean needs re-init ==="

set +e
ERR=$( (cd "$PROJECT" && "$BINARY" run echo hi) 2>&1)
CODE=$?
set -e

if [ "$CODE" != "0" ]; then
	pass "cas run fails after clean (no .sandbox)"
else
	fail "cas run should fail after clean"
fi

# ── summary ───────────────────────────────────────────────────────────────────

rm -rf "$TMP"

echo ""
echo "==================================="
echo "Results: $PASS passed, $FAIL failed"
echo "==================================="

[ "$FAIL" = "0" ] || exit 1
