Implement `syncing/pool.rs` — a bounded reusable connection pool for `SyncClient` used by FUSE handlers.

## Why

Per-request `SyncClient::connect()` introduces repeated Unix socket connect/accept overhead and can be slower than a shared client under medium/high syscall rates.

The pool keeps multiple persistent client connections so concurrent FUSE operations avoid both a global client lock and connect churn.
