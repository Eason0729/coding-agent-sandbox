# CAS — Coding Agent Sandbox: Overview

## What Is CAS?

`cas` is a CLI tool that runs untrusted programs (AI coding agents) inside a controlled filesystem environment. It uses FUSE to intercept filesystem operations and enforce per-path read/write policies without requiring root privileges or container runtimes.

An agent can read the project freely while writes are either redirected to a private fuse store (copy-on-write), passed through to the real filesystem (whitelist), or silently hidden (blocklist).

---

## Core Concepts

| Concept | Description |
|---|---|
| **Sandbox** | An isolated FUSE mount wrapping the **entire host filesystem** (rooted at `/`). The sandboxed process sees the whole host tree through FUSE; per-path policy governs how each subtree behaves. |
| **Policy** | Per-path rules: `hide_real` (fuse-only), `passthrough` (real FS pass-through), `default` (copy-on-write) |
| **Fuse store** | `.sandbox/data/` — private storage backing the FUSE layer |
| **Syncing daemon** | A long-lived process, similar to redis, that generate object/persists fuse data and serializes writes across concurrent instances |
| **FUSE daemon** | A server of FUSE, one per session, that mount fuse and maintain per-session fuse data |
| **SHM** | A POSIX shared-memory segment(basically a Mutex counter) that ensure: 1. only one syncing daemon. 2. the daemon exit when all concurrent processes exit |

---

## High-Level Flow

```
User            cas CLI                       Syncing daemon (forked child)
 |                 |                                     |
 |-- cas init ---->|                                     |
 |               create .sandbox/                        |
 |               write metadata.bin                      |
 |               write config.toml                       |
 |                 |                                     |
 |-- cas run ----->|                                     |
 |              open/create SHM                          |
 |              increment running_count                  |
 |              (if 0→1) fork ─────────────────────────> |
 |                 |         child: adopt mutex          |
 |                 |         bind socket                 |
 |                 |         socket_ready = 1            |
 |              wait socket_ready                        |
 |              release SHM lock                         |
 |              three-process fork                       |
 |                 └── sandbox_init (CLONE_NEWUSER+CLONE_NEWNS)
 |                       ├── fork → FUSE daemon ───────> |
 |                       │     connect syncing server    |
 |                       └── setup process:              |
 |                             prepare_rootfs            |
 |                             chroot + seccomp          |
 |                             exec <cmd>                |
 |              <cmd> exits                              |
 |              decrement running_count                  |
 |                                            shutdown + flush (if count→0)
 |-- cas clean ---->|                                    |
 |              remove .sandbox/data                     |
 |              reset SHM counter                        |
```

---

## Clean Module Architecture

```
src/
├── main.rs                   # Binary: cas CLI entry point
├── syncing/                  # Syncing daemon
│   ├── mod.rs                # Re-exports
│   ├── proto.rs              # SandboxMetadata, FileMetadata, PersistedFuse, Request, Response
│   ├── client.rs             # SyncClient
│   └── server/
│       ├── mod.rs            # Re-exports
│       ├── objects.rs        # ObjectStore: objects/{id} blob files
│       ├── disk.rs           # load/flush fuse + metadata to disk
│       └── ...               # incomplete plan
├── shm/                      # POSIX SHM + process-shared mutex counter - unsafe isolated here
│   ├── mod.rs                # Re-exports
│   ├── region.rs             # ShmRegion: shm_open + mmap lifecycle
│   ├── state.rs              # ShmState: typed accessor over SHM layout
│   └── mutex.rs              # ShmGuard, adopt_mutex_after_fork
├── fuse/                     # FUSE filesystem
│   ├── mod.rs                # Re-exports
│   ├── policy.rs             # Policy trait
│   ├── inode.rs              # InodeTable: bidirectional ino ↔ path map
│   ├── daemon_client.rs      # DaemonClient: framed socket client to cas-daemon
│   ├── mount.rs              # run_fuse(), unmount() — FUSE session lifecycle
│   └── fs.rs                 # implements fuser::Filesystem
├── isolate/                  # Namespace isolaton
│   ├── mod.rs                # Re-exports
│   ├── stage1.rs             # Stage 1 isolation: create user NS
│   ├── stage2.rs             # Stage 2 isolation: mount NS and remaining of security measurements/
│   └── seccomp.rs            # apply_seccomp_filter, drop_capabilities
├── config/                   # Config format
│   ├── mod.rs                # Re-exports: Config, ConfigPolicy
│   ├── policy.rs             # struct and implementor of Policy
│   └── config.rs             # Config struct
└── cli/                      # CLI commands + sandbox setup — runs as cas binary
    ├── mod.rs            # Re-exports
    ├── init.rs           # cmd_init: create .sandbox/ tree
    ├── run.rs            # cmd_run: SHM lifecycle + sandbox launch
    └── clean.rs          # cmd_clean: remove fuse data, reset SHM
```

---

## Data Layout

```
<project-root>/
└── .sandbox/
    ├── data/
    │   ├── metadata.bin      postcard: SandboxMetadata (shm_name, abi_version, next_id)
    │   ├── data.bin          postcard: HashMap<path, FileMetadata>
    │   └── objects/          raw content blobs: objects/<shard>/<id_hex> (shard = low byte)
    ├── config.toml           whitelist/ignorelist/blocklist glob arrays
    ├── access.log            first-access audit log (plain text)
    ├── .gitignore            git ignore for access.log and data
    └── daemon.sock           unix socket (present only while daemon is alive)
```

## Filesystem Policy

> `.sandbox` is implicitly added to the **HideReal** list (unless presented in whitelist config).
> `$(pwd)` (the project root) is implicitly added to the **whitelist** (passthrough), unless presented in the blocklist config.
> The sandbox process current working directory is also implicitly added to the **whitelist** (passthrough), unless presented in the blocklist config.

The FUSE filesystem is rooted at `/` — it presents the **entire host filesystem** to the sandboxed process. Policy rules decide what happens at each path:

Evaluation order: **HideReal → whitelist → default (CoW)**.

There are exactly three access modes:

| Access Mode | Read source | Write destination | Access logged? |
|---|---|---|---|
| **HideReal** | Fuse only (empty store → ENOENT) | Fuse | No |
| **Passthrough** | Real FS | Real FS | No |
| **CopyOnWrite** (default) | Real FS (fuse store if previously written) | Fuse | Yes (first access) |

`.sandbox/` uses **HideReal** — the real directory is hidden; the fuse store starts empty so all reads return ENOENT and writes go only to the fuse store.

`ignorelist` matches follow CopyOnWrite policy but suppress first-access log entries.

---

## Multi-Instance Coordination

The lock-transfer protocol ensures exactly one syncing daemon runs at a time:

1. `cas run` opens/creates SHM, increments `running_count`.
2. If `prev == 0`: fork child process; parent keeps SHM open, waits for `socket_ready`; child:
   - Calls `adopt_mutex_after reinitialize mutex in_fork()` to child
   - Binds Unix socket, sets `socket_ready=1`, then unlocks mutex
3. If `prev > 0`: just wait for `socket_ready`
4. All instances spin-wait on `socket_ready` before proceeding
5. On exit: decrement `running_count`; when it reaches 0, the daemon flushes and exits

**Note**: The syncing daemon runs as a forked child process of `cas run`, not as a separate binary. This avoids the need for inter-process communication via argv/environment and simplifies deployment.

---

## Non-Goals (v1)

- Full container isolation (particularly network)
- Security against kernel exploits
- Hard links (refused with `EPERM`)
- Cross-policy-boundary atomic rename
