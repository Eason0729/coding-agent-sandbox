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

## Lookup/getattr

- For `CopyOnWrite`:
  - if daemon entry exists and is whiteout => `ENOENT`
  - if daemon entry exists and not whiteout => use daemon metadata
  - else fallback to real FS
- For `FuseOnly`: daemon metadata only.
- For `Passthrough`: real FS only.

## open/create behavior

- `open`:
  - if existing daemon file entry has `object_id`: open object path directly.
  - if `CopyOnWrite` read-only and no daemon entry: open real FS directly.
  - otherwise ensure object mapping, copy initial real bytes when needed, open object path.
- `create`:
  - in managed modes, ensure object mapping first, then open object file with create flags.
  - remove exact-path whiteout after successful create.

## readdir merge

Directory entries are merged by direct child name:

1. collect real children (except `FuseOnly` policy)
2. overlay daemon direct children
3. remove names with daemon whiteout entries
4. daemon non-whiteout entries shadow same-name real entries

This guarantees `/home/eason` metadata entry does not implicitly hide `/home/eason/*.txt` on real FS.

## Whiteout semantics

- Whiteout is exact-path tombstone.
- No opaque subtree mode in this version.
- `unlink` in managed modes writes whiteout at exact path.
- `rmdir` in managed modes writes whiteout at removed directory and descendants currently visible from lower layer (deep whiteout) to avoid reappearance.
- `create`/`mkdir`/`symlink`/object-ensure remove exact-path whiteout.

## rename behavior

- file/symlink: `RenameFile` metadata move.
- directory: `RenameTree` metadata subtree move.
- destination whiteout is removed after successful rename.

## Flush/release/fsync

- flush backing file (`sync_data`)
- metadata can be updated via daemon `PutFileMeta` where needed
- no content upload call.
