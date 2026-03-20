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

use crate::fuse::policy::Policy;
use crate::fuse::run_fuse;
use crate::isolate::seccomp::apply_seccomp_filter;
use crate::isolate::stage1::create_user_ns;
use crate::isolate::stage2::{create_mount_ns, drop_capabilities, prepare_chroot};
use crate::shm::ShmState;

use crate::cli::run::RunError;

pub type Result<T> = std::result::Result<T, RunError>;

const FUSE_READY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RunContext {
    pub project_root: PathBuf,
    pub daemon_socket: PathBuf,
    pub shm: ShmState,
    policy: Arc<dyn Policy>,
}

impl RunContext {
    pub fn new(
        project_root: PathBuf,
        daemon_socket: PathBuf,
        shm: ShmState,
        policy: Arc<dyn Policy>,
    ) -> Self {
        Self {
            project_root,
            daemon_socket,
            shm,
            policy,
        }
    }

    pub fn policy(&self) -> Arc<dyn Policy> {
        Arc::clone(&self.policy)
    }
}

pub struct SetupPayload {
    pub rootfs: PathBuf,
    pub mountpoint: PathBuf,
    pub cwd: PathBuf,
    pub daemon_socket: PathBuf,
    pub cmd_argv: Vec<String>,
    pub policy: Arc<dyn Policy>,
    pub pty_slave: Option<PathBuf>,
}

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

fn spawn_setup_process(payload: SetupPayload) -> Result<Pid> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => Ok(child),
        Ok(ForkResult::Child) => run_setup_stage1(payload),
        Err(e) => Err(RunError::Fork(e)),
    }
}

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

fn run_fuse_daemon(mountpoint: PathBuf, daemon_socket: PathBuf, policy: Arc<dyn Policy>) -> ! {
    let fuse_fs = crate::fuse::CasFuseFs::new(daemon_socket, policy);

    if let Err(e) = run_fuse(fuse_fs, &mountpoint) {
        log::error!(
            "fuse.child event=run_failed mountpoint={} error={e}",
            mountpoint.display()
        );
        std::process::exit(1);
    }

    std::process::exit(0);
}

fn run_setup_stage2(payload: SetupPayload, fuse_child: Pid) -> ! {
    if let Err(e) = wait_for_fuse_ready(&payload.mountpoint, fuse_child) {
        log::error!("setup2 event=fuse_not_ready error={e}");
        std::process::exit(1);
    }

    if let Err(e) = prepare_chroot(
        &payload.rootfs,
        &payload.mountpoint,
        &payload.cwd,
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

fn exec_command(argv: &[String], use_pty: bool) -> Result<()> {
    let Some(program) = argv.first() else {
        return Err(RunError::NoCommand);
    };

    if use_pty {
        setsid().map_err(|e| RunError::Pty(format!("setsid failed: {e}")))?;

        let tty = CString::new("/dev/tty").map_err(|_| RunError::InvalidCommandArg)?;
        let tty_fd = unsafe { libc::open(tty.as_ptr(), libc::O_RDWR) };
        if tty_fd < 0 {
            return Err(RunError::Pty(format!(
                "open /dev/tty failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let rc = unsafe { libc::ioctl(tty_fd, libc::TIOCSCTTY as _, 0) };
        if rc < 0 {
            let _ = unsafe { libc::close(tty_fd) };
            return Err(RunError::Pty(format!(
                "TIOCSCTTY failed: {}",
                std::io::Error::last_os_error()
            )));
        }

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
            if unsafe { libc::dup2(tty_fd, stdfd) } < 0 {
                let _ = unsafe { libc::close(tty_fd) };
                return Err(RunError::Pty(format!(
                    "dup2 to fd {stdfd} failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

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

pub fn run_in_sandbox(ctx: &RunContext, cmd_args: &[String]) -> Result<i32> {
    let rootfs = tempfile::tempdir()?;
    let mountpoint = tempfile::tempdir()?;

    let use_pty = isatty(std::io::stdin().as_raw_fd()).unwrap_or(false)
        && isatty(std::io::stdout().as_raw_fd()).unwrap_or(false);

    let mut pty_master: Option<OwnedFd> = None;
    let mut pty_slave_path: Option<PathBuf> = None;
    if use_pty {
        let openpty_result = openpty(None, None).map_err(|e| RunError::Pty(e.to_string()))?;
        let master = &openpty_result.master;
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
        policy: ctx.policy(),
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
        let mut poll_fds = vec![PollFd::new(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(master_raw) },
            PollFlags::POLLIN,
        )];
        if stdin_open {
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
