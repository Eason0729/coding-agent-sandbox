use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use log;
use nix::sys::wait::waitpid;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::unistd::{fork, ForkResult};
use thiserror::Error;

use crate::config::{Config, ConfigPolicy};
use crate::isolate::seccomp::apply_seccomp_filter;
use crate::isolate::stage1::create_user_ns;
use crate::isolate::stage2::{create_mount_ns, drop_capabilities, prepare_chroot};
use crate::shm::ShmState;
use crate::syncing::server::PollLock;
use crate::syncing::SyncClient;

pub type Result<T> = std::result::Result<T, RunError>;

#[derive(Debug, Error)]
pub enum RunError {
    #[error("not initialized — run `cas init` first")]
    NotInitialized,
    #[error("failed to load metadata: {0}")]
    Meta(#[from] crate::syncing::disk::DiskError),
    #[error("failed to load config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("failed to build policy: {0}")]
    Policy(String),
    #[error("SHM error: {0}")]
    Shm(#[from] crate::shm::ShmError),
    #[error("fork error: {0}")]
    Fork(nix::errno::Errno),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stage1 error: {0}")]
    Stage1(#[from] crate::isolate::stage1::Stage1Error),
    #[error("stage2 error: {0}")]
    Stage2(#[from] crate::isolate::stage2::Stage2Error),
    #[error("seccomp error: {0}")]
    Seccomp(#[from] crate::isolate::seccomp::SeccompError),
    #[error("command is required")]
    NoCommand,
    #[error("syncing daemon not ready: {0}")]
    DaemonNotReady(String),
    #[error("FUSE mount failed: {0}")]
    FuseMountFailed(String),
}

/// Poll the mountpoint until a different filesystem is mounted there (i.e.
/// the FUSE daemon has completed its mount).  We detect this by comparing
/// the device number of the mountpoint against its parent: once FUSE mounts,
/// the kernel assigns a new device number to the mountpoint.
fn wait_for_fuse_ready(mountpoint: &Path, fuse_child: nix::unistd::Pid) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(u64::MAX);

    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Ok(meta) = std::fs::metadata(mountpoint) {
            if meta.dev() != parent_dev {
                return Ok(());
            }
        }

        match waitpid(fuse_child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(WaitStatus::Exited(_, code)) => {
                return Err(RunError::FuseMountFailed(format!(
                    "fuse daemon exited before mount with code {code}"
                )));
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                return Err(RunError::FuseMountFailed(format!(
                    "fuse daemon was killed by signal {sig}"
                )));
            }
            Ok(_) => {
                return Err(RunError::FuseMountFailed(
                    "fuse daemon terminated before mount".to_string(),
                ));
            }
            Err(e) => {
                return Err(RunError::FuseMountFailed(format!(
                    "failed to wait fuse daemon: {e}"
                )));
            }
        }

        if Instant::now() > deadline {
            return Err(RunError::FuseMountFailed(
                "timeout waiting for FUSE mount".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_daemon_ready(shm: &ShmState, daemon_socket: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let socket_ready = {
            let guard = unsafe { shm.lock() };
            guard.is_socket_ready()
        };

        if socket_ready && UnixStream::connect(daemon_socket).is_ok() {
            return Ok(());
        }

        if Instant::now() > deadline {
            return Err(RunError::DaemonNotReady(format!(
                "socket not accepting connections at {}",
                daemon_socket.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RunningCountGuard<'a> {
    shm: &'a ShmState,
    armed: bool,
}

impl<'a> RunningCountGuard<'a> {
    fn new(shm: &'a ShmState) -> Self {
        Self { shm, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunningCountGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut guard = unsafe { self.shm.lock() };
        guard.decrement();
    }
}

struct ShmPollLock<'a> {
    shm: &'a ShmState,
    last_check: Instant,
}

impl<'a> ShmPollLock<'a> {
    fn new(shm: &'a ShmState) -> Self {
        Self {
            shm,
            last_check: Instant::now(),
        }
    }
}

impl PollLock for ShmPollLock<'_> {
    fn poll_shutdown<F>(&mut self, on_shutdown: F) -> bool
    where
        F: FnOnce(),
    {
        if self.last_check.elapsed() < Duration::from_secs(1) {
            return false;
        }

        let mut guard = unsafe { self.shm.lock() };
        if guard.get_running_count() != 0 {
            drop(guard);
            self.last_check = Instant::now();
            return false;
        }

        guard.set_socket_ready(false);
        on_shutdown();
        true
    }
}

pub fn cmd_run(project_root: &Path, cmd_args: &[String]) -> Result<()> {
    if cmd_args.is_empty() {
        return Err(RunError::NoCommand);
    }

    let sandbox_dir = project_root.join(".sandbox");
    if !sandbox_dir.exists() {
        return Err(RunError::NotInitialized);
    }

    // 1. Load metadata to get shm_name
    let (meta, _fuse_map) = crate::syncing::disk::load(&project_root.to_path_buf())?;

    // 2. Load config and build policy
    let config_path = sandbox_dir.join("config.toml");
    let config = Config::from_file(&config_path)?;
    let policy = ConfigPolicy::from_config(&config, project_root)
        .map_err(|e| RunError::Policy(e.to_string()))?;
    let policy: Arc<dyn crate::fuse::policy::Policy> = Arc::new(policy);

    // 3. Open or create the POSIX SHM segment
    let shm = match ShmState::open(&meta.shm_name) {
        Ok(s) => s,
        Err(_) => ShmState::create(&meta.shm_name)?,
    };

    let daemon_socket = sandbox_dir.join("daemon.sock");

    let mut guard = unsafe { shm.lock() };
    let was_running = guard.increment();

    let mut running_guard = RunningCountGuard::new(&shm);

    if was_running == 0 {
        guard.set_socket_ready(false);
        let poll_lock = ShmPollLock::new(&shm);
        crate::syncing::server::fork_and_run(project_root.to_path_buf(), poll_lock, move || {
            drop(guard)
        })
        .map_err(RunError::Fork)?;
    } else {
        drop(guard);
    }

    wait_for_daemon_ready(&shm, &daemon_socket, Duration::from_secs(15))?;

    // 6. Create a temporary rootfs and mountpoint
    let rootfs = tempfile::tempdir()?;
    let rootfs_path = rootfs.path().to_path_buf();
    let mountpoint = tempfile::tempdir()?;
    let mountpoint_path = mountpoint.path().to_path_buf();

    let cwd = std::env::current_dir().unwrap_or_else(|_| project_root.to_path_buf());

    let cmd_program = cmd_args[0].clone();
    let cmd_argv: Vec<String> = cmd_args.to_vec();
    let daemon_socket_clone = daemon_socket.clone();
    let policy_clone = Arc::clone(&policy);
    // 7. Second fork: setup(1) process
    let child_pid = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // setup(1): create user namespace and mount namespace
            if let Err(e) = create_user_ns() {
                log::error!("setup(1): failed to create user ns: {}", e);
                std::process::exit(1);
            }
            if let Err(e) = create_mount_ns() {
                log::error!("setup(1): failed to create mount ns: {}", e);
                std::process::exit(1);
            }

            // Third fork: FUSE daemon vs setup(2)
            match unsafe { fork() } {
                Ok(ForkResult::Child) => {
                    // FUSE daemon process
                    let daemon = match SyncClient::connect(&daemon_socket_clone) {
                        Ok(d) => d,
                        Err(e) => {
                            log::error!("FUSE daemon: failed to connect to syncing daemon: {}", e);
                            std::process::exit(1);
                        }
                    };

                    // FUSE root is "/" — the entire host filesystem is served through
                    // FUSE so the sandboxed process sees a complete root tree.
                    let fuse_fs = crate::fuse::CasFuseFs::new(
                        std::path::PathBuf::from("/"),
                        daemon_socket_clone.clone(),
                        daemon,
                        policy_clone,
                    );

                    if let Err(e) = crate::fuse::run_fuse(fuse_fs, &mountpoint_path) {
                        log::error!("FUSE daemon: run_fuse error: {}", e);
                        std::process::exit(1);
                    }
                    std::process::exit(0);
                }
                Ok(ForkResult::Parent { child: fuse_child }) => {
                    // setup(2): wait for FUSE to be ready, then chroot + exec
                    if let Err(e) = wait_for_fuse_ready(&mountpoint_path, fuse_child) {
                        log::error!("setup(2): wait_for_fuse_ready failed: {}", e);
                        std::process::exit(1);
                    }

                    if let Err(e) = prepare_chroot(&rootfs_path, &mountpoint_path, &cwd) {
                        log::error!("setup(2): prepare_chroot failed: {}", e);
                        std::process::exit(1);
                    }

                    if let Err(e) = apply_seccomp_filter() {
                        log::error!("setup(2): seccomp failed: {}", e);
                        std::process::exit(1);
                    }

                    if let Err(e) = drop_capabilities() {
                        log::error!("setup(2): drop_capabilities failed: {}", e);
                        std::process::exit(1);
                    }

                    // exec the command
                    let program = std::ffi::CString::new(cmd_program.as_str()).unwrap();
                    let args: Vec<std::ffi::CString> = cmd_argv
                        .iter()
                        .map(|s| std::ffi::CString::new(s.as_str()).unwrap())
                        .collect();

                    let _ = nix::unistd::execvp(&program, &args);
                    log::error!("execvp failed");
                    std::process::exit(1);
                }
                Err(e) => {
                    log::error!("setup(1): fork for FUSE daemon failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(e) => return Err(RunError::Fork(e)),
    };

    // Parent: wait for setup(1) to finish
    let exit_code = match waitpid(child_pid, None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, sig, _)) => {
            log::error!("child killed by signal: {}", sig);
            1
        }
        Ok(_) => 0,
        Err(e) => {
            log::error!("waitpid error: {}", e);
            1
        }
    };

    // 8. Decrement running_count
    running_guard.disarm();
    {
        let mut guard = unsafe { shm.lock() };
        guard.decrement();
    }

    // Drop tempdir handles (unmounts happen automatically on drop for FUSE with AutoUnmount)
    drop(rootfs);
    drop(mountpoint);

    std::process::exit(exit_code);
}
