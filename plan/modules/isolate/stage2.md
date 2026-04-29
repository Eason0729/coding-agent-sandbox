Implement `isolate/stage2.rs` — create mount namespace, prepare rootfs, chroot, and drop capabilities.

Stage 2 runs in the **child process** (the one that will become the final sandboxed process) after the three-process fork. It sets up the mount namespace, prepares the chroot environment, and applies remaining security restrictions.

## Goals

1. Create a new mount namespace (`CLONE_NEWNS` with `MS_PRIVATE`)
2. Prepare a minimal rootfs in a tempdir:
   - Bind-mount the FUSE mountpoint at the **root** of the chroot (`rootfs/`) — FUSE presents the entire host filesystem, so no system directories need to be separately mounted
   - Bind-mount `/proc` from the host into `rootfs/proc`
   - Bind-mount individual `/dev` device files into `rootfs/dev/`
   - Bind-mount the allocated PTY slave as `rootfs/dev/tty` (if the sandbox runs with PTY enabled)
   - Mount a `tmpfs` at `rootfs/tmp`
3. `chroot` into the tempdir rootfs
4. `chdir` to the original working directory (which is accessible through FUSE passthrough)
5. Drop all Linux capabilities except those required for the sandbox to function

## Context

Stage 2 runs in this process hierarchy (from the overview flow):

```
parent (CLI)
  └── fork → syncing daemon
  └── fork → FUSE daemon (mounts FUSE at tempdir mountpoint)
  └── fork → sandboxed process (stage1 + stage2 here)
```

## Rootfs Layout

The chroot rootfs is a **sparse tempdir**. The FUSE mount covers the entire host filesystem, so it is bind-mounted at the rootfs root:

```
rootfs/          ← bind-mount of FUSE mountpoint (entire host FS via FUSE)
rootfs/proc/     ← bind-mount of host /proc (MS_BIND | MS_REC)
rootfs/dev/null  ← bind-mount of /dev/null  (MS_BIND; target file created first)
rootfs/dev/zero  ← bind-mount of /dev/zero
rootfs/dev/urandom
rootfs/dev/random
rootfs/dev/tty   ← bind-mount of allocated PTY slave from host (if PTY enabled)
rootfs/tmp/      ← tmpfs
```

Because FUSE is bind-mounted at `rootfs/`, all of `/bin`, `/usr`, `/lib`, the project directory, etc. are visible inside the chroot through the FUSE layer.

## Notes

1. Bind mount is allowed (ONLY) after creating user NS, because `CAP_SYS_ADMIN` is gained after that.
2. `/proc` must be bind-mounted from the host (`MS_BIND | MS_REC`), not mounted fresh — a fresh `proc` mount requires owning a PID namespace, which we do not create.
3. Device files must be bind-mounted one-by-one; the target files must be created (`std::fs::File::create`) before the bind mount since the kernel requires file-to-file bind mounts, including `/dev/tty` when PTY mode is enabled.
4. The FUSE bind mount uses `MS_BIND` (no `MS_NOEXEC` — executables must run from it).
5. After `chroot`, `chdir` to the original working directory — it is accessible through FUSE passthrough.
6. The sandbox PTY slave path is bind-mounted as `/dev/tty` only when PTY mode is enabled. The final sandbox process does `setsid()`, opens `/dev/tty` inside the chroot, issues `ioctl(TIOCSCTTY)`, and then `dup2()` to stdin/stdout/stderr before `exec`. The parent relay is responsible for putting the host terminal into raw mode while forwarding input, stripping OSC palette replies before forwarding them, flushing buffered input on teardown, and propagating window size changes via `SIGWINCH` + `TIOCSWINSZ`.
