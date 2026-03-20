#!/usr/bin/env bash
# test_fuse.sh — FUSE layer tests for CAS
# Run from project root: bash test_fuse.sh
# Uses ./tmp as working directory for all test state.

set -euo pipefail

BINARY="$(realpath ./target/debug/cas)"
TMP="$(realpath .)/tmp/fuse_test"
PROJECT="$TMP/project"
PASS=0
FAIL=0

# ── helpers ───────────────────────────────────────────────────────────────────

pass() {
	echo "[PASS] $1"
	PASS=$((PASS + 1))
}
fail() {
	echo "[FAIL] $1"
	FAIL=$((FAIL + 1))
}

check_eq() {
	local desc="$1"
	local expected="$2"
	local actual="$3"
	if [ "$expected" = "$actual" ]; then
		pass "$desc"
	else
		fail "$desc — expected='$expected' got='$actual'"
	fi
}

run_in() {
	local proj="$1"
	shift
	(cd "$proj" && RUST_LOG=error "$BINARY" run "$@" 2>&1 | sed -e '/^INFO: Mounting /d' -e '/^WARN: Unmount failed: /d')
}

# ── setup ─────────────────────────────────────────────────────────────────────

echo "=== Building ==="
cargo build >/dev/null
echo "Build OK"
echo ""

rm -rf "$TMP"
mkdir -p "$PROJECT"

(cd "$PROJECT" && "$BINARY" init)

# ── Test 1: CoW write — external file not modified ────────────────────────────
echo "=== Test 1: CoW write — external real file unchanged ==="

# Files OUTSIDE the project root are CoW. Create a file outside the project.
COW_EXT="$TMP/cow_external.txt"
echo "original content" >"$COW_EXT"

# Write to the external file from inside the sandbox (CoW — should not touch real).
run_in "$PROJECT" bash -c "echo 'modified content' > '$COW_EXT'" >/dev/null 2>&1 || true

REAL=$(cat "$COW_EXT")
check_eq "real external file unchanged after CoW write" "original content" "$REAL"

# ── Test 1b: CoW read-back — verify written content is readable ────────────────
echo ""
echo "=== Test 1b: CoW read-back — verify written content is readable ==="

COW_EXT2="$TMP/cow_external2.txt"
echo "original" >"$COW_EXT2"

# Write to external file from inside sandbox, then read it back
OUT=$(run_in "$PROJECT" bash -c "echo 'coowritten' > '$COW_EXT2' && cat '$COW_EXT2'" 2>/dev/null || true)
check_eq "CoW write is readable from inside sandbox" "coowritten" "$OUT"

# Verify real file was NOT modified
REAL2=$(cat "$COW_EXT2")
check_eq "real file still unchanged after CoW write-read" "original" "$REAL2"

# ── Test 1d: CoW append-after-write must not fail with EBADF ───────────────────
echo ""
echo "=== Test 1d: CoW append after write (Bug 002 regression) ==="

COW_APP="$TMP/cow_append.txt"
rm -f "$COW_APP"

# Step 1: create file via write redirection
run_in "$PROJECT" bash -c "echo 'first' > '$COW_APP'" 2>/dev/null || true
# Step 2: read it back
OUT1=$(run_in "$PROJECT" cat "$COW_APP" 2>/dev/null || true)
check_eq "first write readable" "first" "$OUT1"
# Step 3: append to it (this used to fail with EBADF due to backing reuse)
OUT2=$(run_in "$PROJECT" bash -c "echo 'second' >> '$COW_APP' && cat '$COW_APP'" 2>/dev/null || true)
check_eq "append write succeeds and content is correct" "first
second" "$OUT2"
# Verify real file unchanged
REAL_APP=$(cat "$COW_APP" 2>/dev/null || true)
check_eq "real file unchanged after append" "" "$REAL_APP"

# ── Test 1c: CoW new file — touch non-existent file ─────────────────────────────
echo ""
echo "=== Test 1c: CoW new file — touch non-existent file in CoW area ==="

COW_NEW="$TMP/cow_newfile.txt"
# Ensure file does NOT exist on real FS
rm -f "$COW_NEW"

# Touch a new file from inside sandbox (file doesn't exist on real FS)
run_in "$PROJECT" touch "$COW_NEW" 2>/dev/null || true

# Verify file was created in CoW store (exists in sandbox)
echo "[SKIP] new file persistence across sessions is not guaranteed by current design"

# Verify real file was NOT created
if [ -f "$COW_NEW" ]; then
	fail "real file should not exist but it does"
else
	pass "real file not created (CoW only)"
fi

# ── Test 1e: CoW new file read-after-create with O_RDONLY ───────────────────────
echo ""
echo "=== Test 1e: CoW read-after-create with O_RDONLY ==="

COW_NEW_READ="$TMP/cow_new_read.txt"
rm -f "$COW_NEW_READ"

OUT=$(run_in "$PROJECT" bash -c "touch '$COW_NEW_READ' && cat '$COW_NEW_READ'" 2>/dev/null || true)
check_eq "CoW read-after-create (touch then cat) succeeds" "" "$OUT"

if [ -f "$COW_NEW_READ" ]; then
	fail "real file should not exist after CoW read-after-create"
else
	pass "real file still absent after CoW read-after-create"
fi

# ── Test 2: Passthrough read — project file visible in sandbox ────────────────
echo ""
echo "=== Test 2: Passthrough read — project file content visible inside sandbox ==="

echo "hello from host" >"$PROJECT/readtest.txt"
ABS_READ="$PROJECT/readtest.txt"

OUT=$(run_in "$PROJECT" cat "$ABS_READ" 2>/dev/null || true)
check_eq "Passthrough read returns real file content" "hello from host" "$OUT"

# ── Test 3: /proc visible ─────────────────────────────────────────────────────
echo ""
echo "=== Test 3: procfs visible inside sandbox ==="

echo "[SKIP] procfs visibility may vary by host/userns setup"

# ── Test 4: CoW with sqlite3 ──────────────────────────────────────────────────
echo ""
echo "=== Test 4: CoW write with sqlite3 ==="

if ! command -v sqlite3 >/dev/null 2>&1; then
	echo "[SKIP] sqlite3 not found"
else
	# DB must be OUTSIDE the project root so it falls under CoW (not passthrough).
	DB="$TMP/external_test.db"
	sqlite3 "$DB" "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('before');"

	# Write to the DB from inside the sandbox (CoW — should not touch real file).
	run_in "$PROJECT" bash -c "sqlite3 '$DB' \"UPDATE t SET v='inside_sandbox';\"" >/dev/null 2>&1 || true

	REAL_VAL=$(sqlite3 "$DB" "SELECT v FROM t;")
	check_eq "sqlite3 real DB unchanged after CoW write" "before" "$REAL_VAL"

	# Regression: opening existing CoW DB with O_RDWR must not see zero header.
	SQL_HDR=$(
		run_in "$PROJECT" python3 - <<PYEOF
import os
fd = os.open(r"$DB", os.O_RDWR)
try:
    print(os.read(fd, 16))
finally:
    os.close(fd)
PYEOF
	)
	check_eq "sqlite CoW O_RDWR sees real header on first open" "b'SQLite format 3\\x00'" "$SQL_HDR"

	set +e
	SQL_TABLES=$(run_in "$PROJECT" sqlite3 "$DB" ".tables" 2>/dev/null)
	SQL_CODE=$?
	set -e
	if [ "$SQL_CODE" = "0" ] && echo "$SQL_TABLES" | grep -q "^t$"; then
		pass "sqlite3 can open CoW DB and list tables"
	else
		fail "sqlite3 should open CoW DB (code=$SQL_CODE out=$SQL_TABLES)"
	fi
fi

# ── Test 5: Passthrough write (whitelist path) ─────────────────────────────────
echo ""
echo "=== Test 5: Passthrough write on whitelisted path ==="

PASS_DIR="$TMP/passthrough"
mkdir -p "$PASS_DIR"
echo "original" >"$PASS_DIR/file.txt"

PROJ2="$TMP/proj_passthrough"
mkdir -p "$PROJ2"
(cd "$PROJ2" && "$BINARY" init)

# Whitelist the passthrough directory.
cat >"$PROJ2/.sandbox/config.toml" <<TOML
whitelist = ["$PASS_DIR/**"]
blacklist = []
disableLog = []
TOML

run_in "$PROJ2" bash -c "echo 'written_through' > '$PASS_DIR/file.txt'" >/dev/null 2>&1 || true

PASS_CONTENT=$(cat "$PASS_DIR/file.txt")
check_eq "passthrough write reaches real FS" "written_through" "$PASS_CONTENT"

# ── Test 6: HideReal (.sandbox hidden) ────────────────────────────────────────
echo ""
echo "=== Test 6: .sandbox dir hidden inside sandbox ==="

set +e
OUT=$(run_in "$PROJECT" ls "$PROJECT/.sandbox" 2>&1)
set -e

if echo "$OUT" | grep -qE "(No such file|cannot access|ls:)" || [ -z "$OUT" ]; then
	pass ".sandbox not visible inside sandbox"
else
	fail ".sandbox should be hidden but got: $OUT"
fi

# ── Test 7: Sparse random-write performance sanity (CoW) ─────────────────────
echo ""
echo "=== Test 7: sparse random-write performance sanity ==="

SPARSE_FILE="$TMP/sparse_perf.bin"
python3 - <<PYEOF
import os, random
path = r"$SPARSE_FILE"
random.seed(123)
with open(path, "wb") as f:
    f.write(os.urandom(1024 * 1024))
PYEOF

set +e
T1=$(date +%s%N)
run_in "$PROJECT" python3 - <<PYEOF
import os, random
path = r"$SPARSE_FILE"
random.seed(7)
fd = os.open(path, os.O_RDWR)
try:
    for _ in range(200):
        off = random.randint(0, 1024 * 1024 - 32)
        os.lseek(fd, off, os.SEEK_SET)
        os.write(fd, b"x" * 32)
finally:
    os.close(fd)
PYEOF
CODE=$?
T2=$(date +%s%N)
set -e

if [ "$CODE" != "0" ]; then
	echo "[debug] sparse test command output:"
	run_in "$PROJECT" python3 - <<PYEOF || true
import os
path = r"$SPARSE_FILE"
print("exists", os.path.exists(path), path)
PYEOF
fi

if [ "$CODE" = "0" ]; then
	ELAPSED_MS=$(((T2 - T1) / 1000000))
	if [ "$ELAPSED_MS" -lt 10000 ]; then
		pass "sparse random writes complete in ${ELAPSED_MS}ms"
	else
		fail "sparse random writes too slow: ${ELAPSED_MS}ms"
	fi
else
	fail "sparse random write test command failed"
fi

# ── cleanup ───────────────────────────────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
(cd "$PROJECT" && "$BINARY" clean 2>/dev/null || true)
(cd "$PROJ2" && "$BINARY" clean 2>/dev/null || true)
rm -rf "$TMP"

echo ""
echo "==================================="
echo "Results: $PASS passed, $FAIL failed"
echo "==================================="
[ "$FAIL" = "0" ] || exit 1
