Implement `isolate/seccomp.rs` — apply a seccomp BPF filter to restrict available syscalls.

Seccomp runs in the sandboxed process after chroot but before exec. It uses libseccomp to install a BPF filter that allows a minimal set of syscalls while blocking dangerous ones.

---

## Goals

1. Block syscalls that could be used to escape the sandbox or cause harm
2. Allow syscalls needed for normal program execution (filesystem reads, memory allocation, etc.)
3. Allow syscalls needed for FUSE communication
4. Return `SECCOMP_RET_KILL` for blocked syscalls (process is killed)

---

## Function

```rust
pub fn apply_seccomp_filter() -> Result<()>
```

---

## Syscall Whitelist

The filter allows these syscall categories:

### Core syscalls (always allowed)

| Syscall | Reason |
|---|---|
| `read`, `write`, `writev`, `pread64`, `pwrite64` | File I/O |
| `open`, `openat`, `close`, `stat`, `lstat`, `fstat`, `access`, `faccessat` | File metadata |
| `readlink`, `readlinkat` | Symlink resolution |
| `getcwd`, `getdents64` | Directory listing |
| `mkdir`, `rmdir`, `unlink`, `link`, `symlink`, `rename` | File operations |
| `chmod`, `fchmod`, `chown`, `fchown`, `lchown` | Ownership (no effect outside namespace) |
| `mount`, `umount`, `umount2` | Allowed inside mount namespace |
| `truncate`, `ftruncate` | File size |
| `mmap`, `mprotect`, `munmap`, `mremap`, `msync` | Memory management |
| `brk` | Heap |
| `madvise` | Memory hints |
| `dup`, `dup2`, `dup3` | File descriptor duplication |
| `pipe`, `pipe2` | Pipes |
| `select`, `poll`, `epoll_create`, `epoll_create1`, `epoll_ctl`, `epoll_wait`, `epoll_pwait`, `epoll_pwait2` | I/O multiplexing |
| `pselect6`, `ppoll` | I/O multiplexing with signals |
| `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | Signal handling |
| `sigaltstack` | Signal stack |
| `kill` | Signal sending (own process) |
| `socketcall` / `recvfrom`, `sendto`, `recvmsg`, `sendmsg` | Unix socket for FUSE |
| `getsockopt`, `setsockopt` | Socket options |
| `getpeername`, `getsockname` | Socket names |
| `socket`, `socketpair` | Socket creation |
| `bind`, `listen`, `accept`, `connect` | Socket operations |
| `shmget`, `shmat`, `shmctl` | Shared memory (for FUSE) |
| `clock_gettime`, `clock_getres`, `clock_nanosleep` | Time |
| `gettimeofday`, `time` | Time |
| `nanosleep`, `alarm`, `setitimer` | Timers |
| `getpid`, `getppid`, `getpgrp`, `getsid`, `getuid`, `getgid`, `geteuid`, `getegid` | Process info |
| `setuid`, `setgid`, `seteuid`, `setegid`, `setreuid`, `setregid` | UID/GID (no effect on host) |
| `getgroups`, `setgroups` | Groups |
| `getresuid`, `getresgid`, `setresuid`, `setresgid` | Real/effective IDs |
| `getpgid`, `setpgid`, `setsid` | Session/process group |
| `setfsuid`, `setfsgid` | Filesystem UID/GID (called by bash on startup) |
| `setns` | Namespace entry (allow joining our own namespaces) |
| `uname` | System info |
| `sysinfo` | System info |
| `syslog` | Kernel logging (restricted) |
| `getrlimit`, `setrlimit` | Resource limits |
| `getrusage` | Resource usage |
| `umask` | File mode creation mask |
| `prctl` | Process control (restricted) |
| `prlimit64` | Resource limits |
| `getrandom` | Random bytes |
| `utimensat`, `utimes` | File timestamps (used by `touch`) |
| `execve` | **Allowed** — the command itself may exec subprocesses |
| `exit`, `exit_group` | Exit |
| `wait4`, `waitid` | Process waiting |
| `personality` | Process personality |
| `arch_prctl` | Architecture-specific |

### Explicitly blocked (dangerous)

| Syscall | Reason |
|---|---|
| `kexec_load`, `kexec_file_load` | Kernel execution |
| `init_module`, `delete_module` | Load/unload kernel modules |
| `lookup_dcookie` | Dentry cookies |
| `perf_event_open` | Performance monitoring |
| `quotactl` | Quota control |
| `setxattr`, `lsetxattr`, `fsetxattr`, `removexattr`, `lremovexattr`, `fremovexattr` | Extended attributes (could hide data) |
| `keyctl`, `add_key`, `request_key` | Key management |
| `mbind`, `set_mempolicy`, `get_mempolicy` | NUMA policy |
| `move_pages` | Page migration |
| `name_to_handle_at`, `open_by_handle_at` | File handles |
| `unshare` | Would create new namespaces (escape risk) |
| `create_module`, `query_module` | Kernel modules |
| `get_kernel_syms`, `get_module` | Kernel symbols |
| `iopl`, `ioperm` | I/O port access |
| `idle` | CPU idle |

---

## Notes

- **FUSE communication**: The sandboxed process communicates with the FUSE daemon via a Unix socket. This requires `socket`, `connect`, `sendmsg`, `recvmsg`, etc. The socket is passed as an fd into the chroot (via the bind mount of `/project` which contains the socket).

- **Fork/clone allowed**: `clone`, `clone3` are in the allowlist so the sandboxed process can fork (e.g. bash spawning subcommands). Restricting fork is optional but not currently enforced.

- **Network Allow**: Network syscalls (`socket` with `AF_INET`, `bind` with a port, etc.) are allowed, and the network namespace isn't unshared.

- **Seccomp failure**: If seccomp fails to load (e.g., on older kernels), return an error and abort the sandbox launch. The process must not run without the filter.

---

## Known Bug — Interactive Shell SIGSYS

### Symptom

```
$ cas run bash
child killed by signal: SIGSYS
```

Running an interactive shell (`bash` with no `-c` argument) causes the sandboxed process to be killed by seccomp.

### Root Cause

Interactive bash calls syscalls during startup that are not in the allowlist:

| Syscall | x86_64 # | Reason called |
|---|---|---|
| `epoll_create1` | 291 | readline initializes an epoll-based event loop when stdin is a real TTY |
| `setfsuid` | 122 | bash normalizes filesystem UID at startup |
| `setfsgid` | 123 | bash normalizes filesystem GID at startup |

`epoll_create` (213) was in the allowlist but `epoll_create1` (291) was not — they are distinct syscalls.

### Debugging Method (no root required)

1. Change seccomp default action to `SCMP_ACT_ERRNO(ENOSYS)` — process survives but blocked calls return ENOSYS.
2. Run the failing command to confirm it completes without SIGSYS.
3. Change to `SCMP_ACT_LOG` — blocked calls are logged to the audit subsystem.
4. Re-run and check `journalctl --since "1 minute ago" | grep SECCOMP` for the syscall numbers.
5. Decode: `ausyscall x86_64 <number>`.

> Note: `strace` bypasses seccomp (ptrace intercepts before seccomp fires), so ENOSYS returns are not visible in strace output. Use journalctl with `SCMP_ACT_LOG` instead.

### Fix

Add the three syscalls to the `allow_core_syscalls` list in `isolate/seccomp.rs`.
