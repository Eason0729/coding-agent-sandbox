use caps::{CapSet, Capability};
use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use nix::sched::CloneFlags;
use nix::unistd::{chdir, chroot};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Stage2Error {
    #[error("failed to create mount namespace: {0}")]
    CreateMountNs(Errno),
    #[error("failed to set mount propagation to private: {0}")]
    SetMountPrivate(Errno),
    #[error("failed to mount {src} to {tgt}: {err}")]
    Mount {
        src: String,
        tgt: String,
        err: Errno,
    },
    #[error("failed to create directory: {0}")]
    MountIo(String),
    #[error("failed to chroot to {path}: {error}")]
    Chroot { path: PathBuf, error: Errno },
    #[error("failed to chdir to {path}: {error}")]
    Chdir { path: PathBuf, error: Errno },
    #[error("failed to drop capabilities: {0}")]
    DropCaps(String),
}

pub type Result<T> = std::result::Result<T, Stage2Error>;

pub fn create_mount_ns() -> Result<()> {
    nix::sched::unshare(CloneFlags::CLONE_NEWNS).map_err(Stage2Error::CreateMountNs)?;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(Stage2Error::SetMountPrivate)?;

    Ok(())
}

pub fn prepare_chroot(
    rootfs: &Path,
    mountpoint: &Path,
    cwd: &Path,
    controlling_tty: &Option<PathBuf>,
    pty_slave: &Option<PathBuf>,
) -> Result<()> {
    // Pre-create the mountpoint directories and device file placeholders on the
    // real tempdir BEFORE binding FUSE at the rootfs root.  The FUSE bind-mount
    // will shadow the tempdir contents, but we need the kernel to see the
    // target paths when we later stack /proc, /dev, and tmpfs on top.
    std::fs::create_dir_all(rootfs.join("proc"))
        .map_err(|e| Stage2Error::MountIo(e.to_string()))?;
    std::fs::create_dir_all(rootfs.join("dev")).map_err(|e| Stage2Error::MountIo(e.to_string()))?;
    for dev in ["null", "zero", "random", "urandom"] {
        std::fs::File::create(rootfs.join("dev").join(dev))
            .map_err(|e| Stage2Error::MountIo(e.to_string()))?;
    }

    // 1. Bind FUSE at rootfs/ — it presents the entire host filesystem.
    bind_mount_fuse(mountpoint, rootfs)?;

    // 2. Stack real /proc and /dev nodes on top of FUSE.
    bind_mount_proc(rootfs)?;
    bind_mount_dev(rootfs, controlling_tty, pty_slave)?;

    // 3. chroot into the prepared rootfs.
    chroot(rootfs).map_err(|e| Stage2Error::Chroot {
        path: rootfs.to_path_buf(),
        error: e,
    })?;

    // 4. chdir to the original working directory.  It is accessible inside
    //    the chroot through the FUSE passthrough of the host filesystem.
    //    NOTE: This must happen BEFORE mounting tmpfs at /tmp so that a cwd
    //    under /tmp is still reachable via FUSE.
    chdir(cwd).map_err(|e| Stage2Error::Chdir {
        path: cwd.to_path_buf(),
        error: e,
    })?;

    // 5. Mount a fresh tmpfs at /tmp so that temporary files written inside
    //    the sandbox don't pollute the FUSE CoW store.  Mounted after chdir so
    //    that a cwd under /tmp is not shadowed at step 4.
    bind_mount_tmpfs(Path::new("/tmp"))?;

    Ok(())
}

/// Bind-mount the FUSE mountpoint at the rootfs root so that the entire host
/// filesystem is accessible inside the chroot through the FUSE layer.
fn bind_mount_fuse(mountpoint: &Path, rootfs: &Path) -> Result<()> {
    mount(
        Some(mountpoint),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| Stage2Error::Mount {
        src: mountpoint.display().to_string(),
        tgt: rootfs.display().to_string(),
        err: e,
    })
}

/// Bind-mount the host's /proc into the chroot.
///
/// A fresh `proc` mount requires owning a PID namespace; since we only create
/// a user+mount namespace, we bind-mount the host /proc instead.
fn bind_mount_proc(rootfs: &Path) -> Result<()> {
    let target = rootfs.join("proc");

    mount(
        Some("/proc"),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| Stage2Error::Mount {
        src: "/proc".to_string(),
        tgt: target.display().to_string(),
        err: e,
    })
}

fn bind_mount_dev(
    rootfs: &Path,
    controlling_tty: &Option<PathBuf>,
    pty_slave: &Option<PathBuf>,
) -> Result<()> {
    let dev_devices = ["null", "zero", "urandom", "random"];
    let dev_target = rootfs.join("dev");

    for dev in dev_devices.iter() {
        let source = format!("/dev/{}", dev);
        let target = dev_target.join(dev);

        mount(
            Some(source.as_str()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| Stage2Error::Mount {
            src: source,
            tgt: target.display().to_string(),
            err: e,
        })?;
    }

    if let Some(tty_path) = controlling_tty {
        let target_tty_path = dev_target.join("tty");
        mount(
            Some(tty_path.as_os_str()),
            &target_tty_path,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| Stage2Error::Mount {
            src: tty_path.to_string_lossy().into_owned(),
            tgt: target_tty_path.display().to_string(),
            err: e,
        })?;
    }

    let target_pts_path = dev_target.join("pts");
    std::fs::create_dir_all(&target_pts_path).map_err(|e| Stage2Error::MountIo(e.to_string()))?;
    mount(
        Some("devpts"),
        &target_pts_path,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,mode=666,ptmxmode=666"),
    )
    .map_err(|e| Stage2Error::Mount {
        src: "devpts".to_string(),
        tgt: target_pts_path.display().to_string(),
        err: e,
    })?;

    let target_ptmx_path = dev_target.join("ptmx");
    let source_ptmx_path = target_pts_path.join("ptmx");
    mount(
        Some(source_ptmx_path.as_os_str()),
        &target_ptmx_path,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| Stage2Error::Mount {
        src: source_ptmx_path.display().to_string(),
        tgt: target_ptmx_path.display().to_string(),
        err: e,
    })?;

    if let Some(pty_slave_path) = pty_slave {
        let target_tty_path = dev_target.join("tty");
        mount(
            Some(pty_slave_path.as_os_str()),
            &target_tty_path,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| Stage2Error::Mount {
            src: pty_slave_path.display().to_string(),
            tgt: target_tty_path.display().to_string(),
            err: e,
        })?;
    }

    Ok(())
}

fn bind_mount_tmpfs(target: &Path) -> Result<()> {
    mount(
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .map_err(|e| Stage2Error::Mount {
        src: "tmpfs".to_string(),
        tgt: target.display().to_string(),
        err: e,
    })
}

pub fn drop_capabilities() -> Result<()> {
    for cap in [
        Capability::CAP_CHOWN,
        Capability::CAP_DAC_OVERRIDE,
        Capability::CAP_DAC_READ_SEARCH,
        Capability::CAP_FOWNER,
        Capability::CAP_FSETID,
        Capability::CAP_KILL,
        Capability::CAP_SETGID,
        Capability::CAP_SETUID,
        Capability::CAP_SETPCAP,
        Capability::CAP_LINUX_IMMUTABLE,
        Capability::CAP_NET_BIND_SERVICE,
        Capability::CAP_NET_BROADCAST,
        Capability::CAP_NET_ADMIN,
        Capability::CAP_NET_RAW,
        Capability::CAP_IPC_LOCK,
        Capability::CAP_IPC_OWNER,
        Capability::CAP_SYS_MODULE,
        Capability::CAP_SYS_RAWIO,
        Capability::CAP_SYS_CHROOT,
        Capability::CAP_SYS_PTRACE,
        Capability::CAP_SYS_PACCT,
        Capability::CAP_SYS_ADMIN,
        Capability::CAP_SYS_BOOT,
        Capability::CAP_SYS_NICE,
        Capability::CAP_SYS_RESOURCE,
        Capability::CAP_SYS_TIME,
        Capability::CAP_SYS_TTY_CONFIG,
        Capability::CAP_MKNOD,
        Capability::CAP_LEASE,
        Capability::CAP_AUDIT_WRITE,
        Capability::CAP_AUDIT_CONTROL,
        Capability::CAP_SETFCAP,
    ] {
        let _ = caps::drop(None, CapSet::Effective, cap);
        let _ = caps::drop(None, CapSet::Permitted, cap);
        let _ = caps::drop(None, CapSet::Inheritable, cap);
    }

    Ok(())
}
