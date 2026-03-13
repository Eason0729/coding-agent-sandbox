Implement `isolate/stage1.rs` — create a new user namespace with UID/GID mapping.

Stage 1 runs in the setup(1) process (the second fork child, after the syncing daemon fork). It creates the user namespace that will be inherited by both the FUSE daemon child and the final sandboxed process.

---

## Goals

1. Create a new user namespace (`CLONE_NEWUSER`)
2. Map the current host UID to the same UID inside the namespace (identity mapping)
3. Map the current host GID to the same GID inside the namespace
4. Keep the mapping so that the sandboxed process has its own UID/GID scope but retains the same numeric UID/GID

The identity mapping provides namespace isolation without elevated privileges

## Lifetime

`UserNs` is constructed in the setup(1) process (after the syncing daemon fork). The file handles are inherited by both:
- The FUSE daemon child (third fork)
- The final sandboxed process (setup(2))

Both children therefore operate inside the same user namespace. The handles are closed when the parent process exits.

## Note

You need to read gid/uid **BEFORE** entering stage1! And use that pid to setup mapping.
