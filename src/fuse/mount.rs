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

    let thread_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);

    config.n_threads = Some(thread_count);

    for opt in options {
        config.mount_options.push(opt);
    }

    let session = fuser::spawn_mount2(fs, mountpoint, &config)?;
    session.join()
}
