use std::io;
use std::path::Path;

use fuser::MountOption;

use crate::fuse::fs::CasFuseFs;

/// Mount `fs` at `mountpoint` and block until the filesystem is unmounted.
pub fn run_fuse(fs: CasFuseFs, mountpoint: &Path) -> io::Result<()> {
    // AutoUnmount requires config.acl != SessionACL::Owner, but CUSTOM("allow_other")
    // only pushes a string into mount_options without updating config.acl, which
    // causes a runtime panic.  Both options are unnecessary here: the FUSE daemon
    // runs inside a private mount namespace, so the mount is cleaned up automatically
    // when the namespace is torn down.
    let options = vec![MountOption::FSName("cas".to_string())];

    let mut config = fuser::Config::default();
    for opt in options {
        config.mount_options.push(opt);
    }

    fuser::mount2(fs, mountpoint, &config)
}

/// Unmount the FUSE filesystem at `mountpoint`.
///
/// Called by the parent process after the sandboxed child exits, to tear down
/// the FUSE session that `run_fuse` is blocking on.
pub fn unmount(mountpoint: &Path) -> io::Result<()> {
    // fuser does not expose a standalone unmount helper, so we call the system
    // `fusermount -u` / `umount` directly.
    let status = std::process::Command::new("fusermount")
        .args(["-u", &mountpoint.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(io::Error::other(format!(
            "fusermount -u exited with status {s}"
        ))),
        // fusermount may not be available inside the user namespace; fall back
        // to the plain umount(2) syscall via the `nix` crate.
        Err(_) => {
            nix::mount::umount(mountpoint).map_err(|e| io::Error::from_raw_os_error(e as i32))
        }
    }
}
