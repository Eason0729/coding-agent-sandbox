Implement `cas run` — load metadata and config, manage SHM lifecycle, fork syncing daemon on 0→1, wait for socket readiness, launch the sandbox, and propagate the exit code.

## Auto-Initialization

If `.sandbox/` does not exist, `cas run` will automatically initialize the sandbox before proceeding (same behavior as `cas clean`).

## Implementation Notes (Divergences from Original Spec)

The original specification contained some details that did not match the actual implementation. The following corrections were made during implementation:

### 1. `DaemonClient` Type Does Not Exist

The spec references `DaemonClient` as a type alias in `fuse/daemon_client.rs`, but this type does not exist in the codebase. Instead, use `SyncClient` from `crate::syncing`:

```rust
// Correct import:
use crate::syncing::SyncClient;

// NOT:
use crate::fuse::DaemonClient;  // Does not exist
```

### 2. `CasFuseFs::new` Signature

The actual signature is:
```rust
let fuse_fs = CasFuseFs::new(
    root: PathBuf,       // Must be PathBuf::from("/") — FUSE covers the entire host FS
    daemon: SyncClient,
    policy: Arc<dyn Policy>,
);
```

`root` **must be `PathBuf::from("/")`**. The FUSE filesystem presents the entire host filesystem to the sandboxed process. Policy classifies each path into `Passthrough`, `HideReal`, or `CopyOnWrite`.

### 3. `run_fuse` Signature

The spec shows:
```rust
run_fuse(&mountpoint, fuse_fs, &options)?;
```

The actual signature is (options are hardcoded inside `mount.rs`):
```rust
run_fuse(fuse_fs, &mountpoint)?;
```

### 4. Mount Options

Mount options are hardcoded in `fuse/mount.rs`:
```rust
let options = vec![
    MountOption::FSName("cas".to_string()),
    MountOption::AutoUnmount,
    MountOption::CUSTOM("allow_other".to_string()),
];
```

Do not pass options as a parameter to `run_fuse`.

## Overview

The `cas run` command:
1. Loads `.sandbox/metadata.bin` to get `shm_name`
2. Opens or creates the POSIX SHM segment (`/cas-<shm_name>`)
3. Locks SHM mutex, reads/increments `running_count`, and updates `socket_ready` when needed
4. If `running_count` was 0 before increment: fork a child to run the syncing daemon
5. Waits for `socket_ready` flag in SHM and successful socket connect
6. Forks again to create the sandboxed execution environment
7. On child exit: locks SHM mutex and decrements `running_count`

## Shared-State Rule

Do not use `fetch_add`/`fetch_sub` on `running_count` for run-lifecycle transitions.

`running_count` participates in a larger critical section that also includes daemon startup/shutdown
state (`socket_ready`, fork timing, socket bind readiness). These transitions must be performed while
holding the SHM mutex.

## Runtime Robustness

To avoid stale SHM/socket races and silent hangs:

1. When `running_count` transitions 0→1, clear `socket_ready` before starting a new daemon.
2. `cas run` must wait for both conditions before proceeding:
   - `socket_ready == 1`
   - `UnixStream::connect(daemon.sock)` succeeds
3. Waiting is bounded (timeout) and returns an explicit run error on failure.
4. Timeout should be generous enough to avoid false negatives on busy hosts (15s).

## Running Count Safety

`running_count` must be decremented on every early-return error path after increment.

Use an RAII guard in `cmd_run` so failures in daemon readiness checks or setup paths cannot leak
`running_count` and cause future runs to skip daemon startup incorrectly.

## FUSE Readiness

`wait_for_fuse_ready` must be bounded and detect early daemon death:

1. Poll mountpoint device change as before.
2. Concurrently `waitpid(..., WNOHANG)` on the FUSE daemon child.
3. If the child exits/signals before mount completion, fail fast with a clear error.
4. If timeout is reached, fail with timeout error instead of waiting forever.

## Imports

```rust
use crate::config::{Config, ConfigPolicy};      // re-exported from config/mod.rs
use crate::fuse::{run_fuse, CasFuseFs};          // No DaemonClient in fuse module
use crate::syncing::SyncClient;                  // Use SyncClient, not DaemonClient
```

## `Result` type alias

`run.rs` defines its own `Result<T>` alias with one generic parameter:

```rust
pub type Result<T> = std::result::Result<T, RunError>;
```

All internal functions must use `Result<()>` (one argument), not `Result<(), RunError>` (two arguments).

## Implementation Details

### SHM Lifecycle

```rust
let mut shm = match ShmState::open(&meta.shm_name) {
    Ok(s) => s,
    Err(ENOENT) => ShmState::create(&meta.shm_name)?,  // create if not exists
    Err(e) => return Err(e),
};
```

### Forking Syncing Daemon

When `running_count` transitions 0→1:

```rust
if count_before == 0 {
    match fork() {
        Ok(ForkResult::Child) => {
            // This is the syncing daemon
            // Need to reinitialize pthread mutex after fork
            adopt_mutex_after_fork(shm.state_mut());
            
            // Run syncing server (blocking)
            syncing::server::run(sandbox_dir, shm);
            
            std::process::exit(0);
        }
        Ok(ForkResult::Parent { child }) => {
            // Parent: wait for daemon to be ready
            while !shm.socket_ready() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Err(e) => return Err(e),
    }
}
```

The child process calls `adopt_mutex_after_fork()` to reinitialize the pthread mutex, which is not safe to use after fork without reinitialization.

### FUSE Mount Options

Mount options are hardcoded in `fuse/mount.rs` and are not passed as a parameter to `run_fuse`:

```rust
// In fuse/mount.rs - options are hardcoded:
let options = vec![
    fuser::MountOption::FSName("cas".to_string()),
    fuser::MountOption::AutoUnmount,
    fuser::MountOption::CUSTOM("allow_other".to_string()),
];
```

## Three-Process Fork Pattern

The sandbox uses a three-process fork pattern to properly isolate the FUSE daemon and the sandboxed process:

```
parent (CLI)
  └── fork → syncing daemon
  └── fork → setup(1) process
          └── fork → FUSE daemon (runs FUSE mount)
          └── setup(2) process: chroot + exec
```

### Step 1: First Fork (Syncing Daemon)

Already described above - forks the syncing daemon.

### Step 2: Second Fork (setup(1))

After syncing daemon is ready, fork to create the setup(1) process. When PTY mode is enabled, the parent allocates a PTY pair and passes the slave path through `SetupPayload`:

```rust
// Allocate PTY before fork (in run_in_sandbox)
let openpty_result = openpty(None, None)?;
let pty_slave = ttyname(&openpty_result.slave)?;

match fork() {
    Ok(ForkResult::Child) => {
        // setup(1) process - creates user namespace and mount namespace
        create_user_ns()?;
        create_mount_ns()?;
        
        // Fork again for FUSE daemon and sandboxed process
        match fork() {
            Ok(ForkResult::Child) => {
                // FUSE daemon process
                // Connect to syncing daemon (use SyncClient, not DaemonClient)
                let daemon = SyncClient::connect(&daemon_socket)?;
                
                // Create FUSE filesystem — root is / so FUSE covers the entire host FS
                let fuse_fs = CasFuseFs::new(
                    PathBuf::from("/"),
                    daemon,
                    Arc::new(policy),  // Arc<dyn Policy>, not Box::new
                );
                
                // Run FUSE (blocks until unmounted) - options are hardcoded
                run_fuse(fuse_fs, &mountpoint)?;
                std::process::exit(0);
            }
            Ok(ForkResult::Parent { child: fuse_child }) => {
                // setup(2) process - prepares chroot and exec
                // Wait for FUSE to be ready (poll mountpoint)
                wait_for_fuse_ready(&mountpoint);
                
                // Prepare chroot with /dev, /proc, fuse mount, and /dev/tty (PTY slave bind)
                prepare_chroot(&rootfs, &mountpoint, &cwd, &pty_slave)?;
                
                // Chroot, apply seccomp, drop caps, exec
                chroot_and_exec(&rootfs, &mut cmd)?;
            }
            Err(e) => std::process::exit(1),
        }
    }
    Ok(ForkResult::Parent { .. }) => {
        // Parent: wait for child to complete
    }
    Err(e) => return Err(e),
}
```

The `SetupPayload` struct includes `pty_slave: Option<PathBuf>` which carries the allocated PTY slave path into setup(2).

In setup(2), before `exec`, the process uses the following sequence when PTY is enabled:

1. `setsid()`
2. `open("/dev/tty", O_RDWR)` (requires `rootfs/dev/tty` to be precreated and bind-mounted)
3. `ioctl(slave_fd, TIOCSCTTY, 0)`
4. `dup2(slave_fd, 0/1/2)`
5. `execvp(...)`

The parent process keeps the PTY master and relays I/O between host stdio and the sandbox process. If the host stdin is a TTY, the relay temporarily switches the host stdin to raw mode so user keystrokes are forwarded once instead of being echoed twice by both the host terminal and the child shell, then flushes any buffered input when exiting. The relay also strips OSC palette replies (for example `OSC 10/11/12`) from stdin before forwarding them and handles `SIGWINCH` by copying the host terminal size to the PTY master with `TIOCSWINSZ`.

### Key Points

1. **User namespace must be created in setup(1)** - After the second fork but before forking FUSE daemon, so both inherit the same namespace.

2. **Mount namespace created in setup(1)** - After user namespace, to isolate mount operations.

3. **FUSE daemon runs in child of setup(1)** - Inherits user+mount namespace, mounts FUSE at a temp directory.

4. **setup(2) runs in sibling of FUSE daemon** - Waits for FUSE to be ready, then:
   - Prepares chroot with bind mounts
   - Applies seccomp filter
   - Drops capabilities
   - Executes target program

5. **wait_for_fuse_ready** - Polls the mountpoint until it's populated (nlink > 0) before proceeding.
