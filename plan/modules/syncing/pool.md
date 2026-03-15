Implement `syncing/pool.rs` — a bounded reusable connection pool for `SyncClient` used by FUSE handlers.

## Why

Per-request `SyncClient::connect()` introduces repeated Unix socket connect/accept overhead and can be slower than a shared client under medium/high syscall rates.

The pool keeps multiple persistent client connections so concurrent FUSE operations avoid both a global client lock and connect churn.

## API

```rust
pub struct SyncClientPool {
    // socket path + bounded idle queue + condvar-based wait
}

pub struct PooledSyncClient {
    // RAII checkout; returns connection to pool on Drop
}

impl SyncClientPool {
    pub fn new(sock_path: PathBuf, max_size: usize) -> Self;
    pub fn checkout(&self) -> Result<PooledSyncClient, ClientError>;
}
```

`PooledSyncClient` must expose mutable access to `SyncClient` for existing call sites (`&mut SyncClient`).

## Behavior

1. `checkout()` first tries to reuse an idle connection.
2. If no idle connection exists and pool has not reached `max_size`, it opens a new `SyncClient::connect(sock_path)`.
3. If pool is at capacity, wait on a condition variable until another checkout is dropped.
4. Dropping `PooledSyncClient` returns the connection to idle queue and notifies one waiter.

## Capacity Rule (Resource Management)

The FUSE client-pool capacity should be reasonable for the expected concurrency (e.g., 4-8 connections). The server now uses thread-per-connection, so worker starvation is no longer a concern. However, unbounded connection growth should still be prevented to avoid resource exhaustion.

## Error Handling

- Connection creation errors propagate as `ClientError`.
- If the pool has been poisoned due to mutex panic, return `ClientError::Server` with a clear message.

## Concurrency

- Internal state is protected by `Mutex + Condvar`.
- The pool itself is `Send + Sync` behind `Arc`.
- No additional global lock in FUSE for daemon I/O.

## Tests

Add unit tests for:

1. **reuse after drop**: second checkout reuses the first connection (no second accept).
2. **bounded growth**: opening `max_size` checkouts creates at most `max_size` socket connections.
