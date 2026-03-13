Implement `cas clean` — remove fuse data and reset SHM

## Steps

1. Fail if `.sandbox/` does not exist (not initialized)
2. Load metadata to get `shm_name` before removing data
3. Remove `.sandbox/data/` (includes `metadata.bin`, `data.bin`, `objects/`, `access.log`)
4. Attempt to unlink the SHM segment (if it exists)
5. Remove `daemon.sock` if present

## Implementation Notes

- If the SHM segment doesn't exist (already cleaned or never used), the unlink operation is silently ignored
- The metadata is loaded first to get the `shm_name` before the data directory is deleted
