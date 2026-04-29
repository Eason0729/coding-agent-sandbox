# Bug 005: CoW short write can corrupt Cargo cache artifacts

## Symptom

`cargo build` in sandbox works only when cache is already complete.

When cache needs updates/downloads, users see failures like:

- `database disk image is malformed`
- `failed to unpack package ... unexpected end of file`

## Why this happens

In CoW mode, file writes go through `OpenFile::write_at`.

The previous implementation used a single `write(2)` call and returned the
short count directly. Short writes are legal for `write(2)`, so under load a
single logical FUSE write could commit only a prefix.

This leaves partially-written cache files (including Cargo's sqlite metadata
store and crate archives), which later reads interpret as corruption.

## Root cause in code

- `src/fuse/open_file.rs`: `write_at` used `file.write(data)` instead of
  write-all semantics.

## Fix

Use `write_all` semantics in `write_at` and return `data.len()` only after the
entire buffer is persisted.

## Regression coverage

Add unit coverage in `src/fuse/open_file.rs` that writes a large buffer and
verifies full byte-for-byte persistence.
