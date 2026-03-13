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
	(cd "$proj" && "$BINARY" run "$@" 2>&1)
}

# ── setup ─────────────────────────────────────────────────────────────────────

echo "=== Building ==="
cargo build 2>&1 | grep '^error' && exit 1 || true
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

# ── Test 1c: CoW new file — touch non-existent file ─────────────────────────────
echo ""
echo "=== Test 1c: CoW new file — touch non-existent file in CoW area ==="

COW_NEW="$TMP/cow_newfile.txt"
# Ensure file does NOT exist on real FS
rm -f "$COW_NEW"

# Touch a new file from inside sandbox (file doesn't exist on real FS)
run_in "$PROJECT" touch "$COW_NEW" 2>/dev/null || true

# Verify file was created in CoW store (exists in sandbox)
set +e
OUT=$(run_in "$PROJECT" test -f "$COW_NEW" && echo "exists" 2>/dev/null || echo "notexists")
set -e
check_eq "new file created in CoW store is visible" "exists" "$OUT"

# Verify real file was NOT created
if [ -f "$COW_NEW" ]; then
	fail "real file should not exist but it does"
else
	pass "real file not created (CoW only)"
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

set +e
run_in "$PROJECT" test -f /proc/1/status >/dev/null 2>&1
CODE=$?
set -e

if [ "$CODE" = "0" ]; then
	pass "procfs visible inside sandbox"
else
	fail "procfs not accessible inside sandbox"
fi

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
