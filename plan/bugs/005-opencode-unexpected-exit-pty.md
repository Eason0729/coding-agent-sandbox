# Bug 005: `opencode` exits unexpectedly in sandbox due to broken PTY device wiring

## Reproduction

From a project with CAS initialized:

```bash
cas run opencode
```

Observed behavior:

- `opencode` exits unexpectedly.
- The shell then shows leaked OSC response fragments like `11;rgb:...`.

Minimal syscall-level repro:

```bash
cas run python3 -c 'import pty; pty.openpty()'
```

This fails with:

```text
OSError: out of pty devices
```

or direct open on `/dev/ptmx` fails.

## Root Cause

Stage2 chroot setup only bind-mounts a subset of `/dev` entries (`null`, `zero`, `random`, `urandom`, optional `tty`).

`opencode` requires PTY allocation for TUI startup. PTY allocation depends on:

- `/dev/ptmx`
- `/dev/pts` (devpts)

Without explicit bind-mounts for these, PTY paths are served by the FUSE view and do not provide correct PTY semantics. As a result, `openpty`/`posix_openpt` path fails, `opencode` exits, and pending OSC replies can leak back to the shell prompt.

## Fix Plan

1. In stage2 rootfs preparation, ensure `/dev/pts` exists as a directory target.
2. Mount a fresh `devpts` at `rootfs/dev/pts` with `newinstance,mode=666,ptmxmode=666`.
3. Bind-mount `rootfs/dev/pts/ptmx` to `rootfs/dev/ptmx`.
4. Keep existing `/dev/tty` bind behavior for controlling terminal passthrough.
5. Add regression test that performs `pty.openpty()` inside `cas run`.
