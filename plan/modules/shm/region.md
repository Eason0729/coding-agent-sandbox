Implement `shm/region.rs` — `ShmRegion`, the low-level wrapper around POSIX shared memory.

This module handles the raw `shm_open` / `shm_unlink` / `mmap` lifecycle. All operations are unsafe and must be contained here.

---

## Goals

1. Create or open a POSIX shared memory segment by name
2. Map it into the process address space with `mmap`
3. Ensure the mapping is process-shared (for mutex coordination across processes)
4. Clean up on drop
