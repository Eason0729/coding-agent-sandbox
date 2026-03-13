Implement `syncing/client.rs` — `SyncClient`, a typed framed-socket client used by FUSE daemons (and optionally the CLI) to communicate with the syncing daemon.

## Struct

```rust
pub struct SyncClient {
    stream: UnixStream,
}
```

## Constructor

```rust
impl SyncClient {
    /// Connect to the daemon socket. Blocks until the socket file exists
    /// and the connection succeeds (the caller must have already spun on
    /// `socket_ready` before calling this).
    pub fn connect(sock_path: &Path) -> Result<Self>
}
```
