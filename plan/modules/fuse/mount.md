Implement `fuse/mount.rs` — FUSE session lifecycle helpers.

## Lifecycle

`run_fuse` is called from the FUSE daemon process (the child forked inside the user namespace). It blocks until the mounted filesystem is unmounted (triggered by the sandboxed process exiting and `unmount` being called by the parent).

## Performance Update (v2)

Use background mounting API (`fuser::spawn_mount2`) and keep the returned `BackgroundSession` alive for the daemon process lifetime.

This enables fuser multithreaded request serving while preserving current lifecycle semantics.
