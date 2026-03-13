use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use log;
use nix::sys::wait::waitpid;
use nix::sys::wait::WaitStatus;
use nix::unistd::{fork, ForkResult};
use thiserror::Error;

use crate::config::{Config, ConfigPolicy};
use crate::isolate::seccomp::apply_seccomp_filter;
use crate::isolate::stage1::create_user_ns;
use crate::isolate::stage2::{create_mount_ns, drop_capabilities, prepare_chroot};
use crate::shm::{adopt_mutex_after_fork, ShmState};
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
}

/// Poll the mountpoint until a different filesystem is mounted there (i.e.
/// the FUSE daemon has completed its mount).  We detect this by comparing
/// the device number of the mountpoint against its parent: once FUSE mounts,
/// the kernel assigns a new device number to the mountpoint.
fn wait_for_fuse_ready(mountpoint: &Path) {
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(u64::MAX);

    loop {
        if let Ok(meta) = std::fs::metadata(mountpoint) {
            if meta.dev() != parent_dev {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
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

    // 4. Increment running_count atomically
    let count_before = shm.increment_running_count();

    let daemon_socket = sandbox_dir.join("daemon.sock");

    // 5. If running_count was 0 before increment: fork the syncing daemon
    if count_before == 0 {
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                // Syncing daemon process
                // Reinitialize pthread mutex after fork
                let mut shm_child = match ShmState::open(&meta.shm_name) {
                    Ok(s) => s,
                    Err(_) => {
                        log::error!("Syncing daemon: failed to open SHM");
                        std::process::exit(1);
                    }
                };
                unsafe {
                    if let Err(e) = adopt_mutex_after_fork(shm_child.state_mut()) {
                        log::error!("Syncing daemon: failed to adopt mutex: {}", e);
                        std::process::exit(1);
                    }
                }

                crate::syncing::server::run(project_root.to_path_buf(), shm_child);
                std::process::exit(0);
            }
            Ok(ForkResult::Parent { .. }) => {
                // Parent: wait for daemon socket to be ready
                loop {
                    if shm.socket_ready() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Err(e) => return Err(RunError::Fork(e)),
        }
    } else {
        // Daemon already running; wait for socket if not yet ready
        loop {
            if shm.socket_ready() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
    let project_root_buf = project_root.to_path_buf();

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
                        daemon,
                        policy_clone,
                    );

                    if let Err(e) = crate::fuse::run_fuse(fuse_fs, &mountpoint_path) {
                        log::error!("FUSE daemon: run_fuse error: {}", e);
                        std::process::exit(1);
                    }
                    std::process::exit(0);
                }
                Ok(ForkResult::Parent { child: _fuse_child }) => {
                    // setup(2): wait for FUSE to be ready, then chroot + exec
                    wait_for_fuse_ready(&mountpoint_path);

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
    shm.decrement_running_count();

    // Drop tempdir handles (unmounts happen automatically on drop for FUSE with AutoUnmount)
    drop(rootfs);
    drop(mountpoint);

    std::process::exit(exit_code);
}
