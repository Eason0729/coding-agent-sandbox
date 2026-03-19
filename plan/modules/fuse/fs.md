# fuse/fs.rs — FUSE Orchestration with Overlay Merge

## Overview

`fs.rs` implements `fuser::Filesystem` and handles:

- policy-based path routing (`Passthrough` / `FuseOnly` / `CopyOnWrite`)
- inode/path bookkeeping
- metadata daemon interaction
- merged directory view and whiteout behavior

`open_file.rs` now handles only direct backing-file I/O.

## Backing model

- Real files: normal host FS path.
- Managed files: object-store file path on host FS, obtained from syncing daemon (`object_id -> object path`).
- Protocol carries metadata only; file bytes are never sent over daemon socket.

## Passthrough backing handle invariant

> Each `open` or `create` FUSE call produces its own independent backing file description. Backed handles are **never shared or reused** across opens for the same path.

This is required because different open flags (read-only, write-only, append, truncate) produce file descriptions with different permissions. Sharing a read-only backing fd for a later write/append open would cause EBADF. See `plan/bugs/002-passthrough-backoff-reuse.md`.

## rename behavior

- file/symlink: `RenameFile` metadata move.
- directory: `RenameTree` metadata subtree move.
- destination whiteout is removed after successful rename.

## Flush/release/fsync

- flush backing file (`sync_data`)
- metadata can be updated via daemon `PutFileMeta` where needed
