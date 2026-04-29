# fuse/fs.rs — FUSE Adapter and Operation Dispatch

## Overview

`fs.rs` implements `fuser::Filesystem` and is intentionally thin.

`fs.rs` handles:

- request/reply adaptation at the FUSE boundary
- inode/path bookkeeping and open-handle lifecycle
- operation flow orchestration (`load state -> decide -> execute`)

Routing decisions and state classification are moved out of `fs.rs` into pure modules.

## Backing model

- Real files: normal host FS path.
- Managed files: object-store file path on host FS, obtained from syncing daemon (`object_id -> object path`).
- Protocol carries metadata only; file bytes are never sent over daemon socket.

## State/Decision architecture

For each operation, `fs.rs` follows a strict sequence:

1. Build operation state with `state_loader.rs` (always includes daemon read columns, even for `Passthrough`).
2. Select behavior variant with `decision.rs` pure tables.
3. Apply side effects with `executor.rs`.

For `open`, `fs.rs` also captures an ordered transition trace from
`decision.rs` and emits debug logs. This trace is side-effect free and can be
validated in unit tests to guard against timing-sensitive regressions.

This eliminates ad-hoc branching and makes behavior testable without mounting FUSE.

## Passthrough backing handle invariant

> Each `open` or `create` FUSE call produces its own independent backing file description. Backed handles are **never shared or reused** across opens for the same path.

This is required because different open flags (read-only, write-only, append, truncate) produce file descriptions with different permissions. Sharing a read-only backing fd for a later write/append open would cause EBADF. See `plan/bugs/002-passthrough-backoff-reuse.md`.

## rename behavior

- file/symlink: `RenameFile` metadata move.
- directory: `RenameTree` metadata subtree move.
- destination whiteout is removed after successful rename.

## readdir behavior source of truth

`readdir` child merge policy lives in `decision.rs` with explicit child-level outcomes,
including:

- real-only child
- fuse-only child
- both-present collision
- whiteout masking
- `Passthrough` collision `DontCare` classification

## export support path resolution

When `FUSE_EXPORT_SUPPORT` is enabled, `lookup` must resolve special components
consistently:

- `.` resolves to the current directory path
- `..` resolves to the parent directory path

This keeps export-style lookups aligned with kernel expectations.

## Flush/release/fsync

- flush backing file (`sync_data`)
- metadata can be updated via daemon `PutFileMeta` where needed

## SQLite sidecar handling

- SQLite rollback-journal files must not bypass FUSE lock/fsync semantics.
- First write-capable open of an existing sqlite DB must copy-up the real file
  contents before publishing the object entry.
- Rollback journal creation/deletion must remain on the normal FUSE path so
  lock ordering and `fsyncdir` behavior match sqlite expectations.
