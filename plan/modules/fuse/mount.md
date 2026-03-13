Implement `fuse/mount.rs` — FUSE session lifecycle helpers.

## Lifecycle

`run_fuse` is called from the FUSE daemon process (the child forked inside the user namespace). It blocks until the mounted filesystem is unmounted (triggered by the sandboxed process exiting and `unmount` being called by the parent).
