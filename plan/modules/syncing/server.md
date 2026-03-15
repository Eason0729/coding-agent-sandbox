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

Thread-per-connection model:

1. Main thread accepts `UnixStream` connections.
2. For each incoming connection, spawn a dedicated thread to run `handle_connection` until EOF.
3. Each connection is fully independent — idle persistent connections (from client connection pools) never block other connections.

**Why not bounded worker pool:** The previous bounded worker pool model could cause worker starvation when multiple sandbox instances ran concurrently. Each persistent client connection (from `SyncClientPool`) holds a worker thread indefinitely while waiting for future requests. With only 4 workers, one sandbox's pool could occupy all workers, leaving another sandbox's connection requests unscheduled.

Thread-per-connection ensures every incoming connection makes progress regardless of other connections' state.

## Concurrency and Locking

- Metadata map uses a per-row concurrent hashmap (dashmap).
- Access log remains a single mutex-guarded file append.
- Object storage keeps a mutex for object-id allocation and file mutation, while metadata lookups/updates are per-row.
- Lock ordering rule: never hold metadata row guards while waiting on the object-store mutex.

## Protocol Update (v2)

Server supports partial object/file IO primitives:

- `GetObjectRange { id, offset, len }`
- `PatchFile { path, patches, truncate_to }`

These are the canonical write path for FUSE dirty ranged files.

## Shutdown

Shutdown is triggered when `shm_state.running_count` reaches 0, polled in the main accept loop (not per-connection thread).

**Shutdown steps:**
1. Break the accept loop (main thread).
2. Send shutdown signal to all active connection threads (optional: let them finish current request).
3. Call `disk::flush(sandbox_dir, &meta, &fuse_map)` (equivalent to handling a `Flush` request).
4. Remove `daemon.sock`.
5. Exit the process.

## Error Handling

All `Result` errors in request dispatch are converted to `Response::Error(msg)`. The server never panics on a bad client request.
