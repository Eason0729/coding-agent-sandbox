Implement syncing daemon server as metadata authority with object-id allocation.

## Responsibilities

- Own in-memory path map: `HashMap<PathBuf, FuseEntry>` (stored as concurrent map in impl).
- Own object-id allocator and object file creation.
- Persist metadata map and allocator state to disk.
- Never receive or return file content bytes.

## Startup

1. Load metadata snapshot (`disk::load`).
2. Build `ObjectStore` with loaded `next_id`.
3. Open access log.
4. Bind unix socket.
5. Serve requests.

## Protocol handling rules

- `EnsureFileObject`:
  - if file entry with object exists: return existing object.
  - else allocate id, create empty object file, insert file entry.
- `GetObjectPath`:
  - return object real path if object exists.
- `PutDir` / `PutSymlink` / `PutWhiteout`:
  - overwrite exact path entry type.
- `DeleteWhiteout`:
  - only remove entry when exact path is whiteout.
- `RenameTree`:
  - move exact root and all descendants.
- `ReadDirAll`:
  - return direct children only.

## Locking

- metadata map: per-entry concurrency.
- object store mutex: allocator + object existence/path operations.
- do not hold metadata row guards while waiting object lock.

## Whiteout behavior

- Whiteout is exact-path tombstone.
- No implicit opaque subtree mode.
- FUSE layer owns merged readdir semantics.

## Shutdown

- Flush map + metadata to disk.
- Remove socket.

## Error handling

- Invalid/missing state -> `Response::Error(msg)` or `NotFound` where specified.
- Never panic on client input.
