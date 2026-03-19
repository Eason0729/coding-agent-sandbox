Implement syncing daemon server as metadata authority with object-id allocation.

## Responsibilities

- Own in-memory path map: `HashMap<PathBuf, FuseEntry>` (stored as concurrent map in impl).
- Own object-id allocator and object file creation.
- Persist metadata map and allocator state to disk.
- Never receive or return file content bytes.
