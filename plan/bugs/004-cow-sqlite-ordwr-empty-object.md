# Bug 004: CoW sqlite opened with `O_RDWR` sees empty object and fails

## Symptom

Reproducer:

```bash
cd <project>
cas run sqlite3 ../test.db
sqlite> .tables
Error: file is not a database
```

`../test.db` is a valid SQLite file on host and opens correctly outside sandbox.

## Why this happens

In CoW mode, the first write-capable open (`O_RDWR`/`O_WRONLY`) routed to
`ensure_file_object`, which allocates a new empty object when no object entry
exists yet.

For existing host files (like sqlite databases), this means:

1. First `O_RDWR` open creates empty CoW object.
2. Open target switches to that empty object.
3. Reader sees `\x00...` at offset 0 instead of `SQLite format 3\0`.
4. SQLite reports `file is not a database`.

## Root cause in code

- `src/fuse/fs.rs`: CoW `open()` write path always called `ensure_file_object`
  and then opened the returned object path.
- `src/syncing/server.rs`: `EnsureFileObject` allocates empty object for new
  entries.

This behavior is valid for brand-new files, but incorrect for existing real
files that require copy-up semantics.

## Fix

In CoW `open()` write path (`src/fuse/fs.rs`):

- detect when this is first write open (`had_object == false`)
- if real path exists and `O_TRUNC` is not requested, copy real file content
  into newly allocated object (`fs::copy(real, object)`)
- then continue opening the object backing as before

`O_TRUNC` skips copy-up, preserving truncate semantics.

## Regression coverage

Added checks in `tests/test_fuse.sh` sqlite section:

1. `O_RDWR` first-read header must be `b'SQLite format 3\\x00'`
2. `sqlite3 <db> .tables` in sandbox must succeed
3. host DB remains unchanged
