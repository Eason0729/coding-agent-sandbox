use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log;
use nix::poll::{poll, PollFd, PollFlags};
use nix::pty::openpty;
use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, isatty, read, setsid, ttyname, ForkResult, Pid};
use thiserror::Error;

use crate::config::{Config, ConfigPolicy};
use crate::fuse::policy::Policy;
use crate::isolate::seccomp::apply_seccomp_filter;
use crate::isolate::stage1::create_user_ns;
use crate::isolate::stage2::{create_mount_ns, drop_capabilities, prepare_chroot};
use crate::shm::ShmState;
use crate::syncing::server::PollLock;

const FUSE_READY_TIMEOUT: Duration = Duration::from_secs(10);
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

/// Immutable runtime inputs used by `cmd_run` once configuration loading succeeds.
struct RunContext {
    project_root: PathBuf,
    daemon_socket: PathBuf,
    shm: ShmState,
    policy: Arc<dyn Policy>,
}

/// Transient payload used by setup(1)/setup(2) and FUSE child processes.
struct SetupPayload {
    rootfs: PathBuf,
    mountpoint: PathBuf,
    cwd: PathBuf,
    daemon_socket: PathBuf,
    cmd_argv: Vec<String>,
    policy: Arc<dyn Policy>,
    controlling_tty: Option<PathBuf>,
    pty_slave: Option<PathBuf>,
}

/// RAII lease for the `running_count` slot.
///
/// The lease is armed after incrementing `running_count`. If any early-return error
/// happens before the explicit success-path decrement, dropping this lease will perform
/// the decrement under SHM lock and avoid leaked run slots.
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

/// SHM-backed implementation of daemon shutdown polling.
///
/// The key property is lock ownership during shutdown transition:
/// 1. Acquire SHM mutex.
/// 2. Observe `running_count == 0`.
/// 3. Set `socket_ready = 0`.
/// 4. Execute server shutdown callback while lock is still held.
///
/// This removes the transition gap where a new runner could race in while an old
/// daemon is still in the middle of teardown.
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

/// Poll mountpoint readiness and fail fast on early FUSE daemon death.
fn wait_for_fuse_ready(mountpoint: &Path, fuse_child: Pid) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(u64::MAX);

    let deadline = Instant::now() + FUSE_READY_TIMEOUT;

    loop {
        if let Ok(meta) = std::fs::metadata(mountpoint) {
            if meta.dev() != parent_dev {
                log::debug!("run.fuse event=ready mountpoint={}", mountpoint.display());
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

/// Wait until shared-state and socket probe both indicate daemon readiness.
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

/// Load all run-time inputs (metadata, policy, shared memory, socket paths).
fn prepare_context(project_root: &Path) -> Result<RunContext> {
    let sandbox_dir = project_root.join(".sandbox");
    if !sandbox_dir.exists() {
        log::info!("run.lifecycle event=auto_init");
        crate::cli::cmd_clean(project_root, true)
            .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
    }

    let (meta, _fuse_map) = crate::syncing::disk::load(&project_root.to_path_buf())?;
    let policy = build_policy(project_root, &sandbox_dir)?;

    let shm = match ShmState::open(&meta.shm_name) {
        Ok(s) => s,
        Err(_) => ShmState::create(&meta.shm_name)?,
    };

    Ok(RunContext {
        project_root: project_root.to_path_buf(),
        daemon_socket: sandbox_dir.join("daemon.sock"),
        shm,
        policy,
    })
}

/// Build policy object from `.sandbox/config.toml`.
fn build_policy(project_root: &Path, sandbox_dir: &Path) -> Result<Arc<dyn Policy>> {
    let config_path = sandbox_dir.join("config.toml");
    let config = Config::from_file(&config_path)?;
    let policy = ConfigPolicy::from_config(&config, project_root)
        .map_err(|e| RunError::Policy(e.to_string()))?;
    Ok(Arc::new(policy))
}

/// Enter run lifecycle:
/// - increment `running_count` under lock,
/// - start syncing daemon on 0->1 without lock gap,
/// - wait until daemon is externally reachable.
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

/// Leave run lifecycle by decrementing `running_count` under lock.
fn leave_run_lifecycle(shm: &ShmState) {
    let mut guard = unsafe { shm.lock() };
    guard.decrement();
    log::info!("run.lifecycle event=decrement");
}

/// Spawn setup(1) process and return its PID in the parent.
fn spawn_setup_process(payload: SetupPayload) -> Result<Pid> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => Ok(child),
        Ok(ForkResult::Child) => run_setup_stage1(payload),
        Err(e) => Err(RunError::Fork(e)),
    }
}

/// setup(1): create namespaces, then fork into FUSE daemon + setup(2).
fn run_setup_stage1(payload: SetupPayload) -> ! {
    if let Err(e) = create_user_ns() {
        log::error!("setup1 event=user_ns_failed error={e}");
        std::process::exit(1);
    }
    if let Err(e) = create_mount_ns() {
        log::error!("setup1 event=mount_ns_failed error={e}");
        std::process::exit(1);
    }

    match unsafe { fork() } {
        Ok(ForkResult::Child) => run_fuse_daemon(
            payload.mountpoint.clone(),
            payload.daemon_socket.clone(),
            Arc::clone(&payload.policy),
        ),
        Ok(ForkResult::Parent { child: fuse_child }) => run_setup_stage2(payload, fuse_child),
        Err(e) => {
            log::error!("setup1 event=fork_fuse_failed error={e}");
            std::process::exit(1);
        }
    }
}

/// FUSE child: connect daemon client and serve mount loop.
fn run_fuse_daemon(mountpoint: PathBuf, daemon_socket: PathBuf, policy: Arc<dyn Policy>) -> ! {
    let fuse_fs = crate::fuse::CasFuseFs::new(daemon_socket.clone(), policy);

    if let Err(e) = crate::fuse::run_fuse(fuse_fs, &mountpoint) {
        log::error!(
            "fuse.child event=run_failed mountpoint={} error={e}",
            mountpoint.display()
        );
        std::process::exit(1);
    }

    std::process::exit(0);
}

/// setup(2): wait FUSE mount, prepare chroot, apply hardening, exec target command.
fn run_setup_stage2(payload: SetupPayload, fuse_child: Pid) -> ! {
    if let Err(e) = wait_for_fuse_ready(&payload.mountpoint, fuse_child) {
        log::error!("setup2 event=fuse_not_ready error={e}");
        std::process::exit(1);
    }

    if let Err(e) = prepare_chroot(
        &payload.rootfs,
        &payload.mountpoint,
        &payload.cwd,
        &payload.controlling_tty,
        &payload.pty_slave,
    ) {
        log::error!("setup2 event=prepare_chroot_failed error={e}");
        std::process::exit(1);
    }

    if let Err(e) = apply_seccomp_filter() {
        log::error!("setup2 event=seccomp_failed error={e}");
        std::process::exit(1);
    }

    if let Err(e) = drop_capabilities() {
        log::error!("setup2 event=drop_caps_failed error={e}");
        std::process::exit(1);
    }

    if exec_command(&payload.cmd_argv, payload.pty_slave.is_some()).is_err() {
        std::process::exit(1);
    }

    std::process::exit(1);
}

/// Execute target command with `execvp` (never returns on success).
fn exec_command(argv: &[String], use_pty: bool) -> Result<()> {
    let Some(program) = argv.first() else {
        return Err(RunError::NoCommand);
    };

    if use_pty {
        setsid().map_err(|e| RunError::Pty(format!("setsid failed: {e}")))?;

        let tty = CString::new("/dev/tty").map_err(|_| RunError::InvalidCommandArg)?;
        // SAFETY: Calling libc open with a valid C string and flags.
        let tty_fd = unsafe { libc::open(tty.as_ptr(), libc::O_RDWR) };
        if tty_fd < 0 {
            return Err(RunError::Pty(format!(
                "open /dev/tty failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // SAFETY: ioctl TIOCSCTTY is called on an open tty file descriptor.
        let rc = unsafe { libc::ioctl(tty_fd, libc::TIOCSCTTY as _, 0) };
        if rc < 0 {
            // SAFETY: tty_fd is valid after successful open.
            let _ = unsafe { libc::close(tty_fd) };
            return Err(RunError::Pty(format!(
                "TIOCSCTTY failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Disable ECHO on the slave to prevent the slave's line discipline
        // from echoing input back to the master (which would cause double output).
        // SAFETY: borrow_raw creates a BorrowedFd from a valid raw fd.
        let mut termios = {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(tty_fd) };
            match tcgetattr(&borrowed) {
                Ok(t) => t,
                Err(e) => {
                    let _ = unsafe { libc::close(tty_fd) };
                    return Err(RunError::Pty(format!("tcgetattr on slave failed: {e}")));
                }
            }
        };
        termios.local_flags.remove(
            LocalFlags::ECHO
                | LocalFlags::ECHOKE
                | LocalFlags::ECHOE
                | LocalFlags::ECHOK
                | LocalFlags::ECHONL,
        );
        if let Err(e) = {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(tty_fd) };
            tcsetattr(&borrowed, SetArg::TCSANOW, &termios)
        } {
            let _ = unsafe { libc::close(tty_fd) };
            return Err(RunError::Pty(format!("tcsetattr on slave failed: {e}")));
        }

        for stdfd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: dup2 with valid fds.
            if unsafe { libc::dup2(tty_fd, stdfd) } < 0 {
                // SAFETY: tty_fd is valid after successful open.
                let _ = unsafe { libc::close(tty_fd) };
                return Err(RunError::Pty(format!(
                    "dup2 to fd {stdfd} failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        // SAFETY: tty_fd is valid and no longer needed after dup2.
        let _ = unsafe { libc::close(tty_fd) };
    }

    let program = CString::new(program.as_str()).map_err(|_| RunError::InvalidCommandArg)?;
    let mut args = Vec::with_capacity(argv.len());
    for arg in argv {
        args.push(CString::new(arg.as_str()).map_err(|_| RunError::InvalidCommandArg)?);
    }

    match nix::unistd::execvp(&program, &args) {
        Ok(_) => Ok(()),
        Err(e) => {
            log::error!("setup2 event=exec_failed error={e}");
            Err(RunError::Io(std::io::Error::other(e.to_string())))
        }
    }
}

/// Wait for setup(1) completion and normalize process status into exit code.
fn wait_setup_exit(child_pid: Pid) -> i32 {
    match waitpid(child_pid, None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, sig, _)) => {
            log::error!("run.child event=signaled signal={sig}");
            1
        }
        Ok(_) => 0,
        Err(e) => {
            log::error!("run.child event=wait_failed error={e}");
            1
        }
    }
}

/// Build temp rootfs/mountpoint, run setup flow, and return target command exit code.
fn run_in_sandbox(ctx: &RunContext, cmd_args: &[String]) -> Result<i32> {
    let rootfs = tempfile::tempdir()?;
    let mountpoint = tempfile::tempdir()?;

    let controlling_tty = ttyname(&std::io::stdin()).ok();
    let use_pty = isatty(std::io::stdin().as_raw_fd()).unwrap_or(false)
        && isatty(std::io::stdout().as_raw_fd()).unwrap_or(false);

    let mut pty_master: Option<OwnedFd> = None;
    let mut pty_slave_path: Option<PathBuf> = None;
    if use_pty {
        let openpty_result = openpty(None, None).map_err(|e| RunError::Pty(e.to_string()))?;
        let master = &openpty_result.master;
        // Disable ECHO on PTY master to prevent double-output.
        // We keep ICANON (canonical/line-buffered mode) enabled so that
        // \r -> \n conversion and line editing work properly.
        let mut termios =
            tcgetattr(master).map_err(|e| RunError::Pty(format!("tcgetattr failed: {e}")))?;
        termios.local_flags.remove(
            LocalFlags::ECHO
                | LocalFlags::ECHOKE
                | LocalFlags::ECHOE
                | LocalFlags::ECHOK
                | LocalFlags::ECHONL,
        );
        tcsetattr(master, SetArg::TCSANOW, &termios)
            .map_err(|e| RunError::Pty(format!("tcsetattr failed: {e}")))?;
        let slave_path = ttyname(&openpty_result.slave)
            .map_err(|e| RunError::Pty(format!("resolve slave tty path failed: {e}")))?;
        pty_master = Some(openpty_result.master);
        pty_slave_path = Some(slave_path);
    }

    let payload = SetupPayload {
        rootfs: rootfs.path().to_path_buf(),
        mountpoint: mountpoint.path().to_path_buf(),
        cwd: std::env::current_dir().unwrap_or_else(|_| ctx.project_root.clone()),
        daemon_socket: ctx.daemon_socket.clone(),
        cmd_argv: cmd_args.to_vec(),
        policy: Arc::clone(&ctx.policy),
        controlling_tty,
        pty_slave: pty_slave_path,
    };

    let child_pid = spawn_setup_process(payload)?;
    if let Some(master_fd) = pty_master {
        relay_pty_io(master_fd, child_pid)
    } else {
        Ok(wait_setup_exit(child_pid))
    }
}

fn relay_pty_io(master_fd: OwnedFd, child_pid: Pid) -> Result<i32> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let stdout_fd = std::io::stdout().as_raw_fd();
    let master_raw = master_fd.as_raw_fd();
    let mut stdin_open = true;
    let mut child_exit: Option<i32> = None;
    let mut buf = [0u8; 8192];

    loop {
        // SAFETY: borrowing valid FDs for poll duration only.
        let mut poll_fds = vec![PollFd::new(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(master_raw) },
            PollFlags::POLLIN,
        )];
        if stdin_open {
            // SAFETY: borrowing valid FDs for poll duration only.
            poll_fds.push(PollFd::new(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(stdin_fd) },
                PollFlags::POLLIN,
            ));
        }

        poll(&mut poll_fds, 50u16).map_err(|e| RunError::Pty(format!("poll failed: {e}")))?;

        if child_exit.is_none() {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => child_exit = Some(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    log::error!("run.child event=signaled signal={sig}");
                    child_exit = Some(1);
                }
                Ok(_) => {}
                Err(e) => return Err(RunError::Pty(format!("waitpid failed: {e}"))),
            }
        }

        if poll_fds[0]
            .revents()
            .unwrap_or(PollFlags::empty())
            .contains(PollFlags::POLLIN)
        {
            match read(master_raw, &mut buf) {
                Ok(0) => {
                    return Ok(child_exit.unwrap_or_else(|| wait_setup_exit(child_pid)));
                }
                Ok(n) => write_all_fd(stdout_fd, &buf[..n])?,
                Err(e) => {
                    return Err(RunError::Pty(format!("read from pty master failed: {e}")));
                }
            }
        }

        if stdin_open
            && poll_fds
                .get(1)
                .and_then(|p| p.revents())
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN)
        {
            match read(stdin_fd, &mut buf) {
                Ok(0) => stdin_open = false,
                Ok(n) => write_all_fd(master_raw, &buf[..n])?,
                Err(e) => {
                    return Err(RunError::Pty(format!("read from stdin failed: {e}")));
                }
            }
        }

        if let Some(code) = child_exit {
            return Ok(code);
        }
    }
}

fn write_all_fd(fd: i32, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        // SAFETY: Passing valid pointer/len pair to libc::write.
        let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(RunError::Pty(format!("write failed: {err}")));
        }
        let written = written as usize;
        data = &data[written..];
    }
    Ok(())
}

/// Entry point for `cas run`.
///
/// The function intentionally follows lifecycle stages with explicit logs:
/// 1. prepare context
/// 2. enter run lifecycle + daemon readiness
/// 3. launch isolated process tree
/// 4. leave run lifecycle and forward exit code
pub fn cmd_run(project_root: &Path, cmd_args: &[String]) -> Result<()> {
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
