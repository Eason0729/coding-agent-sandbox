use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log;
use thiserror::Error;

use crate::config::{Config, ConfigPolicy};
use crate::fuse::policy::Policy;
use crate::shm::ShmState;
use crate::syncing::server::PollLock;

use crate::cli::sandbox::{run_in_sandbox, Result as SandboxResult, RunContext};

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    #[error("command argument contains NUL byte")]
    InvalidCommandArg,
    #[error("PTY setup failed: {0}")]
    Pty(String),
}

struct RunningCountLease<'a> {
    shm: &'a ShmState,
    armed: bool,
}

impl<'a> RunningCountLease<'a> {
    fn new(shm: &'a ShmState) -> Self {
        Self { shm, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunningCountLease<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        log::warn!("run.lifecycle event=rollback_decrement reason=drop_guard");
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
        if self.last_check.elapsed() < SHUTDOWN_POLL_INTERVAL {
            return false;
        }

        self.last_check = Instant::now();

        let mut guard = unsafe { self.shm.lock() };
        let running = guard.get_running_count();
        log::debug!("sync.lifecycle event=poll running_count={running}");

        if running != 0 {
            return false;
        }

        log::info!("sync.lifecycle event=shutdown_begin reason=running_count_zero");
        guard.set_socket_ready(false);
        on_shutdown();
        log::info!("sync.lifecycle event=shutdown_complete");
        true
    }
}

fn wait_for_daemon_ready(shm: &ShmState) -> Result<()> {
    loop {
        let socket_ready = {
            let guard = unsafe { shm.lock() };
            guard.is_socket_ready()
        };

        if socket_ready {
            log::info!("run.lifecycle event=daemon_ready",);
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

fn prepare_context(project_root: &Path) -> Result<RunContext> {
    let sandbox_dir = project_root.join(".sandbox");
    if !sandbox_dir.exists() {
        log::info!("run.lifecycle event=auto_init");
        crate::cli::cmd_clean(project_root, true)
            .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
    }

    let (meta, _fuse_map, _path_tree) = crate::syncing::disk::load(&project_root.to_path_buf())?;
    let policy = build_policy(project_root, &sandbox_dir)?;

    let shm = match ShmState::open(&meta.shm_name) {
        Ok(s) => s,
        Err(_) => ShmState::create(&meta.shm_name)?,
    };

    Ok(RunContext::new(
        project_root.to_path_buf(),
        sandbox_dir.join("daemon.sock"),
        shm,
        policy,
    ))
}

fn build_policy(project_root: &Path, sandbox_dir: &Path) -> Result<Arc<dyn Policy>> {
    let config_path = sandbox_dir.join("config.toml");
    let config = Config::from_file(&config_path)?;
    let cwd = std::env::current_dir().ok();
    let policy = ConfigPolicy::from_config(&config, project_root, cwd.as_deref())
        .map_err(|e| RunError::Policy(e.to_string()))?;
    Ok(Arc::new(policy))
}

fn enter_run_lifecycle(ctx: &RunContext) -> Result<RunningCountLease<'_>> {
    let mut guard = unsafe { ctx.shm.lock() };
    let previous = guard.increment();

    log::info!("run.lifecycle event=increment previous={previous}");

    let lease = RunningCountLease::new(&ctx.shm);
    if previous == 0 {
        log::info!("run.lifecycle event=daemon_start_begin");
        guard.set_socket_ready(false);

        let poll_lock = ShmPollLock::new(&ctx.shm);
        crate::syncing::server::fork_and_run(ctx.project_root.clone(), poll_lock, move || {
            guard.set_socket_ready(true);
            log::info!("run.lifecycle event=daemon_start_committed socket_ready=1");
            drop(guard);
        })
        .map_err(RunError::Fork)?;
    } else {
        drop(guard);
    }

    wait_for_daemon_ready(&ctx.shm)?;
    Ok(lease)
}

fn leave_run_lifecycle(shm: &ShmState) {
    let mut guard = unsafe { shm.lock() };
    guard.decrement();
    log::info!("run.lifecycle event=decrement");
}

pub fn cmd_run(project_root: &Path, cmd_args: &[String]) -> SandboxResult<i32> {
    if cmd_args.is_empty() {
        return Err(RunError::NoCommand);
    }

    let ctx = prepare_context(project_root)?;
    log::info!("run.lifecycle event=begin root={}", project_root.display());

    let mut lease = enter_run_lifecycle(&ctx)?;
    let exit_code = run_in_sandbox(&ctx, cmd_args)?;

    lease.disarm();
    leave_run_lifecycle(&ctx.shm);

    log::info!("run.lifecycle event=end exit_code={exit_code}");
    std::process::exit(exit_code);
}
