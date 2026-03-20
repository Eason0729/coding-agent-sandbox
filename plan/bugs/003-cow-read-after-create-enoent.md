# Bug 003: CoW file created in sandbox cannot be reopened read-only

## Symptom

Reproducer:

```bash
cargo run -- run bash -c "touch ../y.txt && cat ../y.txt"
```

Observed:

- `touch` succeeds
- `cat` fails with `No such file or directory`

## Why this happens

For paths outside the project root, policy is `CopyOnWrite`.

Flow:

1. `touch ../y.txt` creates a new CoW object entry in daemon metadata/object store.
2. The real host path (e.g. `/project/eason/y.txt`) is intentionally not created.
3. Later, `cat ../y.txt` opens the file with read-only flags (`O_RDONLY`).
4. FUSE `open()` in `CopyOnWrite` mode routed read-only opens to the real path whenever `need_write == false`.
5. Because the real file does not exist, host `open(2)` returns `ENOENT`.

This creates an internal inconsistency: metadata/getattr can see the CoW file, but read-only `open()` ignores it.

## Root cause in code

Old behavior in `src/fuse/fs.rs` (`Filesystem::open`, `AccessMode::CopyOnWrite`):

- when `!need_write`, always used `(path.clone(), None)` (real FS path)
- did not check whether a CoW object already exists for this path

## Correct behavior

In `CopyOnWrite` mode:

- if a CoW object entry exists, both read-only and read-write opens must use object backing
- if no CoW object exists and open is read-only, fallback to real path is valid

## Fix

`src/fuse/fs.rs` now does:

- `need_write == false` + object exists => open object path
- `need_write == false` + no object => open real path
- object id present but object path missing => return `EIO`

## Regression coverage

Added `tests/test_fuse.sh` case:

- `touch` then `cat` for a new CoW file must succeed
- real host file must still remain absent
