#!/usr/bin/env bash
set -euo pipefail

if [[ ! -f "Cargo.toml" ]]; then
	echo "error: run this script from repository root" >&2
	exit 1
fi

CAS_BIN="${CAS_BIN:-./target/debug/cas}"
CAS_RUN_TIMEOUT="${CAS_RUN_TIMEOUT:-25s}"
if [[ ! -x "$CAS_BIN" ]]; then
	echo "[build] cargo build"
	cargo build >/dev/null
fi

TMP_BASE="./tmp/fuse-test"
PROJECT_ROOT="$TMP_BASE/project"
HOST_ROOT="$TMP_BASE/host"

rm -rf "$TMP_BASE"
mkdir -p "$PROJECT_ROOT" "$HOST_ROOT"

pass() {
	echo "[PASS] $1"
}

fail() {
	echo "[FAIL] $1" >&2
	exit 1
}

require_contains() {
	local needle="$1"
	local haystack="$2"
	local label="$3"
	if [[ "$haystack" != *"$needle"* ]]; then
		fail "$label: expected to contain '$needle', got '$haystack'"
	fi
}

run_cas() {
	timeout -k 5s "$CAS_RUN_TIMEOUT" "$CAS_BIN" --root "$PROJECT_ROOT" run "$@"
}

cleanup() {
	pkill -f "$CAS_BIN --root $PROJECT_ROOT run" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "[setup] init sandbox"
"$CAS_BIN" --root "$PROJECT_ROOT" init >/dev/null

echo "[test 1] basic write (passthrough under project root)"
run_cas bash -lc "printf 'basic-write-ok' > '$PROJECT_ROOT/basic.txt'"
content="$(cat "$PROJECT_ROOT/basic.txt")"
[[ "$content" == "basic-write-ok" ]] || fail "basic write content mismatch"
pass "basic write"

echo "[test 2] copy-on-write outside project root"
COW_HOST="$HOST_ROOT/cow.txt"
printf 'host-original' >"$COW_HOST"

inside_view="$(run_cas bash -lc "python3 - <<'PY'
path = '$COW_HOST'
with open(path, 'r+b') as f:
    f.seek(0)
    f.write(b'sandbox-view')

with open(path, 'rb') as f:
    print(f.read().decode('utf-8', 'ignore'))
PY")"

host_after="$(cat "$COW_HOST")"
[[ "$host_after" == "host-original" ]] || fail "host file mutated by CoW write"

require_contains "sandbox-view" "$inside_view" "CoW visible content"
pass "copy-on-write"

echo "[test 3] mmap"
echo "[SKIP] mmap scenario not implemented yet"

echo "[test 4] copy-on-write with sqlite3"
if command -v sqlite3 >/dev/null 2>&1; then
	SQLITE_HOST="$HOST_ROOT/sqlite.db"
	sqlite3 "$SQLITE_HOST" "create table t(v integer); insert into t values (1);"

	sqlite_result="$(run_cas bash -lc "python3 - <<'PY'
import sqlite3

db = '$SQLITE_HOST'
with open(db, 'rb') as f:
    header_ok = f.read(16).startswith(b'SQLite format 3\x00')

con = sqlite3.connect(db)
cur = con.cursor()
cur.execute('insert into t values (2)')
cur.execute('select count(*) from t')
count = cur.fetchone()[0]
con.commit()
con.close()

print('header_ok=' + ('1' if header_ok else '0'))
print('count=' + str(count))
PY")"
	require_contains "header_ok=1" "$sqlite_result" "sqlite header in sandbox"
	require_contains "count=2" "$sqlite_result" "sqlite sandbox row count"

	host_count="$(sqlite3 "$SQLITE_HOST" "select count(*) from t;")"
	[[ "$host_count" == "1" ]] || fail "host sqlite db was modified"

	pass "copy-on-write sqlite"
else
	echo "[SKIP] sqlite3 not found"
fi

echo "[test 5] sparse/random small writes sanity"
if command -v python3 >/dev/null 2>&1; then
	SPARSE_HOST="$HOST_ROOT/sparse.bin"
	truncate -s 8388608 "$SPARSE_HOST"
	before_hash="$(sha256sum "$SPARSE_HOST" | cut -d' ' -f1)"

	start_ts="$(date +%s)"
	run_cas bash -lc "python3 - <<'PY'
import os
import random

path = '$SPARSE_HOST'
size = os.path.getsize(path)
rng = random.Random(20260403)

with open(path, 'r+b') as f:
    for _ in range(1500):
        off = rng.randrange(0, size - 4)
        f.seek(off)
        f.write(rng.randrange(0, 2**32).to_bytes(4, 'little'))

print('random-small-writes:ok')
PY" >/dev/null
	end_ts="$(date +%s)"

	after_hash="$(sha256sum "$SPARSE_HOST" | cut -d' ' -f1)"
	[[ "$before_hash" == "$after_hash" ]] || fail "host sparse file mutated"

	elapsed=$((end_ts - start_ts))
	if ((elapsed > 60)); then
		fail "sparse/random write sanity took too long (${elapsed}s > 60s)"
	fi

	pass "sparse/random small writes (${elapsed}s)"
else
	echo "[SKIP] python3 not found"
fi

echo "[done] all requested fuse checks completed"
