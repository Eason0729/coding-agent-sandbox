Implement the syncing daemon server — `syncing/server/objects.rs`, `syncing/server/disk.rs`, and `syncing/server/mod.rs`.

## Startup Sequence

1. Receive the SHM guard (adopted by `shm::adopt_mutex_after_fork` after fork).
2. Call `disk::load(sandbox_dir)` → `(meta, fuse_map)`.
3. Construct `ObjectStore { dir: sandbox_dir/data/objects, next_id: meta.next_id }`.
4. Open `access.log` for appending.
5. Bind `UnixListener` on `{sandbox_dir}/daemon.sock` (mode 0o600).
6. Set `shm_state.socket_ready = 1` (sequentially consistent store).
7. Drop the SHM guard (unlock mutex so waiting `cas run` processes proceed).
8. Enter the accept loop.

## Accept Loop

Single-threaded. Writes are serialised; no concurrent handlers needed.

## Shutdown

Shutdown is triggered when `shm_state.running_count` reaches 0 (checked after each `handle_connection`).

**Shutdown steps:**
1. Break the accept loop.
2. Call `disk::flush(sandbox_dir, &meta, &fuse_map)` (equivalent to handling a `Flush` request).
3. Remove `daemon.sock`.
4. Exit the process.

## Error Handling

All `Result` errors in request dispatch are converted to `Response::Error(msg)`. The server never panics on a bad client request.
