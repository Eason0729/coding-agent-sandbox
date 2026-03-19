# Bug 002: Passthrough Backing-Id Reuse Across Different Open Modes

> Append write after write/create fails with EBADF when a read-only open happened first.

## Symptom

```bash
$ echo aaa > ../y.txt      # write - OK
$ cat ../y.txt             # read - OK
aaa
$ echo aa >> ../y.txt      # append write - EBADF "Bad file descriptor"
```

The second open (with `O_APPEND`) gets a stale backing fd that was originally opened read-only, causing the kernel to report EBADF on write.

## Root Cause

**Path-only backing-id reuse**: `src/fuse/fs.rs` caches and reuses a `BackingId` by `path` only:

- `open` (line ~808): if backing already exists for path, reuse it
- `create` (line ~904/943): same pattern

This ignores that different open flags (`O_RDONLY` vs `O_WRONLY|O_APPEND`) produce different file descriptions with different permissions. When a read-only open is first to open a CoW path and becomes the cached backing, a later append write receives that same read-only fd handle — causing EBADF on write.

## Affected Code

- `src/fuse/fs.rs:806-813` — open: reuse path-only backing
- `src/fuse/fs.rs:822-826` — open: insert backing by path
- `src/fuse/fs.rs:900-909` — create: same reuse pattern
- `src/fuse/fs.rs:941-945` — create: same insertion pattern

## Fix Specification

**Strategy**: Disable backing-id reuse entirely for passthrough handles. Each `open`/`create` gets its own fresh backing fd.

### Changes

1. **`src/fuse/fs.rs`** — in `open` and `create`:
   - Remove the path-only reuse path: stop checking `inner.backing_ids.get(&path)` before opening.
   - Always call `reply.open_backing(file)` for each open.
   - Do NOT insert into `inner.backing_ids`.

2. **`src/fuse/fs.rs`** — `forget`:
   - Since we no longer share a single backing per path, `forget` should not try to remove from `backing_ids`.
   - Remove the `backing_ids.remove(&path)` call.

3. **`src/fuse/inner.rs`** — `Inner` struct:
   - If `backing_ids` map becomes unused, remove it from the struct.
   - Remove `backing_ids: DashMap<PathBuf, Arc<BackingId>>` field.

4. **`src/fuse/inner.rs`** — `Inner::new`:
   - Remove `backing_ids: DashMap::new()` initialization.

## Invariant

> Each FUSE `open` or `create` call gets its own backing file description. Backed handles are NOT shared or reused across opens, even for the same path.

This matches the FUSE passthrough semantics: each open file description is independent.

## Status

**PLANNED** — implementation pending.