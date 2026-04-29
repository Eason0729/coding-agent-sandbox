use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fuser::OpenFlags;

use crate::error::{Error, Result};
use crate::fuse::decision::{
    CreateDecision, MkdirDecision, OpenDecision, ReadlinkDecision, RenameDecision, RmdirDecision,
    UnlinkDecision,
};
use crate::fuse::open_file::OpenFile;
use crate::fuse::policy::AccessMode;
use crate::syncing::proto::{EntryType, FileMetadata};
use crate::syncing::PooledSyncClient;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct OpenExecResult {
    pub file: std::fs::File,
    pub object_id: Option<u64>,
}

pub fn execute_open(
    decision: OpenDecision,
    client: &mut PooledSyncClient,
    path: &Path,
    flags: OpenFlags,
    default_meta: FileMetadata,
) -> Result<OpenExecResult> {
    let mut options = fs::OpenOptions::new();
    match flags.acc_mode() {
        fuser::OpenAccMode::O_RDONLY => {
            options.read(true);
        }
        fuser::OpenAccMode::O_WRONLY => {
            options.read(true).write(true);
        }
        fuser::OpenAccMode::O_RDWR => {
            options.read(true).write(true);
        }
    }
    if (flags.0 & libc::O_APPEND) != 0 {
        options.append(true);
    }

    match decision {
        OpenDecision::NotFound => Err(Error::from(std::io::Error::from_raw_os_error(libc::ENOENT))),
        OpenDecision::Error => Err(Error::from(std::io::Error::from_raw_os_error(libc::EIO))),
        OpenDecision::OpenReal => {
            let file = options.open(path).map_err(Error::from)?;
            Ok(OpenExecResult {
                file,
                object_id: None,
            })
        }
        OpenDecision::OpenObject {
            existing_object_id,
            needs_ensure,
            copy_up_from_real,
            delete_whiteout,
        } => {
            let (oid, object_path) = if needs_ensure {
                if copy_up_from_real {
                    client
                        .ensure_file_object_from_real(path.to_path_buf(), default_meta.clone())
                        .map_err(Error::from)?
                } else {
                    client
                        .ensure_file_object(path.to_path_buf(), default_meta.clone())
                        .map_err(Error::from)?
                }
            } else {
                let oid = existing_object_id
                    .ok_or_else(|| Error::from(std::io::Error::from_raw_os_error(libc::EIO)))?;
                let op = client.get_object_path(oid).map_err(Error::from)?;
                (oid, op)
            };

            if copy_up_from_real {
                let _ = fs::copy(path, &object_path);
            }
            if delete_whiteout {
                let _ = client.delete_whiteout(path.to_path_buf());
            }

            let file = options.open(&object_path).map_err(Error::from)?;
            Ok(OpenExecResult {
                file,
                object_id: Some(oid),
            })
        }
    }
}

pub fn execute_create(
    decision: CreateDecision,
    client: &mut PooledSyncClient,
    path: &Path,
    mode: u32,
    flags: OpenFlags,
    meta: FileMetadata,
) -> Result<OpenFile> {
    let mut opts = fs::OpenOptions::new();
    match flags.acc_mode() {
        fuser::OpenAccMode::O_RDONLY => {
            opts.read(true);
        }
        fuser::OpenAccMode::O_WRONLY => {
            opts.read(true).write(true);
        }
        fuser::OpenAccMode::O_RDWR => {
            opts.read(true).write(true);
        }
    }
    if (flags.0 & libc::O_APPEND) != 0 {
        opts.append(true);
    }
    opts.create(true).mode(mode);

    match decision {
        CreateDecision::CreateReal => {
            let file = opts.open(path).map_err(Error::from)?;
            Ok(OpenFile::PassthroughReal { file })
        }
        CreateDecision::CreateObject => {
            let (oid, object_path) = client
                .ensure_file_object(path.to_path_buf(), meta)
                .map_err(Error::from)?;
            let _ = client.delete_whiteout(path.to_path_buf());
            let file = opts.open(&object_path).map_err(Error::from)?;
            Ok(OpenFile::PassthroughObject {
                file,
                object_id: oid,
            })
        }
    }
}

pub fn execute_unlink(
    decision: UnlinkDecision,
    client: &mut PooledSyncClient,
    path: &Path,
) -> Result<()> {
    match decision {
        UnlinkDecision::RemoveReal => fs::remove_file(path).map_err(Error::from),
        UnlinkDecision::Whiteout => {
            let _ = client.delete_file(path.to_path_buf());
            client.put_whiteout(path.to_path_buf()).map_err(Error::from)
        }
    }
}

pub fn execute_rmdir(
    decision: RmdirDecision,
    client: &mut PooledSyncClient,
    path: &Path,
    descendants: &[PathBuf],
) -> Result<()> {
    match decision {
        RmdirDecision::RemoveReal => fs::remove_dir(path).map_err(Error::from),
        RmdirDecision::WhiteoutRecursive => {
            let _ = client.delete_file(path.to_path_buf());
            client
                .put_whiteout(path.to_path_buf())
                .map_err(Error::from)?;
            for p in descendants {
                let _ = client.put_whiteout(p.clone());
            }
            Ok(())
        }
    }
}

pub fn execute_rename(
    decision: RenameDecision,
    client: &mut PooledSyncClient,
    from: &Path,
    to: &Path,
) -> Result<()> {
    match decision {
        RenameDecision::RenameReal => fs::rename(from, to).map_err(Error::from),
        RenameDecision::RenameFuseFileOrSymlink => {
            client
                .rename_file(from.to_path_buf(), to.to_path_buf())
                .map_err(Error::from)?;
            let _ = client.delete_whiteout(to.to_path_buf());
            Ok(())
        }
        RenameDecision::RenameFuseTree => {
            client
                .rename_tree(from.to_path_buf(), to.to_path_buf())
                .map_err(Error::from)?;
            let _ = client.delete_whiteout(to.to_path_buf());
            Ok(())
        }
    }
}

pub fn execute_symlink(
    access_mode: AccessMode,
    client: &mut PooledSyncClient,
    path: &Path,
    target: &Path,
    meta: FileMetadata,
) -> Result<()> {
    match access_mode {
        AccessMode::Passthrough => std::os::unix::fs::symlink(target, path).map_err(Error::from),
        AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
            client
                .put_symlink(
                    path.to_path_buf(),
                    target.as_os_str().as_encoded_bytes().to_vec(),
                    meta,
                )
                .map_err(Error::from)?;
            let _ = client.delete_whiteout(path.to_path_buf());
            Ok(())
        }
    }
}

pub fn is_dir_entry(entry: Option<&crate::syncing::proto::FuseEntry>) -> bool {
    entry
        .map(|e| e.entry_type == EntryType::Dir)
        .unwrap_or(false)
}

pub fn execute_setattr_on_open_handle(
    path: &Path,
    fh: crate::fuse::open_file::OpenFile,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    client: &mut PooledSyncClient,
) -> Result<crate::fuse::open_file::OpenFile> {
    let mut of = fh;
    if let Some(sz) = size {
        of.as_mut().set_len(sz).map_err(Error::from)?;
    }
    if mode.is_some() || uid.is_some() || gid.is_some() {
        let mut perms = of.as_mut().metadata().map_err(Error::from)?.permissions();
        if let Some(m) = mode {
            perms.set_mode(m & 0o7777);
            of.as_mut().set_permissions(perms).map_err(Error::from)?;
        }
        let _ = nix::unistd::fchown(
            of.as_mut().as_raw_fd(),
            uid.map(nix::unistd::Uid::from_raw),
            gid.map(nix::unistd::Gid::from_raw),
        );
    }
    if let Ok(meta) = of.as_mut().metadata() {
        let fmeta = FileMetadata {
            size: meta.len(),
            mode: meta.mode(),
            uid: meta.uid(),
            gid: meta.gid(),
            mtime: now_unix(),
            atime: now_unix(),
            ctime: now_unix(),
        };
        let _ = client.put_file_meta(path.to_path_buf(), fmeta);
    }
    Ok(of)
}

pub fn execute_setattr_meta_update(
    client: &mut PooledSyncClient,
    path: &Path,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
) -> Result<()> {
    if let Some(mut m) = client
        .get_file_meta(path.to_path_buf())
        .map_err(Error::from)?
    {
        if let Some(v) = mode {
            m.mode = (m.mode & !0o7777) | (v & 0o7777);
        }
        if let Some(v) = uid {
            m.uid = v;
        }
        if let Some(v) = gid {
            m.gid = v;
        }
        if let Some(v) = size {
            m.size = v;
        }
        m.ctime = now_unix();
        client
            .put_file_meta(path.to_path_buf(), m)
            .map_err(Error::from)?;
    }
    Ok(())
}

pub fn execute_readlink(
    decision: ReadlinkDecision,
    client: &mut PooledSyncClient,
    path: &Path,
) -> Result<Vec<u8>> {
    match decision {
        ReadlinkDecision::UseFuse => {
            let entry = client.get_entry(path.to_path_buf()).map_err(Error::from)?;
            Ok(entry.and_then(|e| e.symlink_target).unwrap_or_default())
        }
        ReadlinkDecision::UseReal => Ok(std::fs::read_link(path)
            .map_err(Error::from)?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()),
        ReadlinkDecision::NotFound => {
            Err(Error::from(std::io::Error::from_raw_os_error(libc::ENOENT)))
        }
    }
}

pub fn execute_mkdir(
    decision: MkdirDecision,
    client: &mut PooledSyncClient,
    path: &Path,
    mode: u32,
    uid: u32,
    gid: u32,
) -> Result<()> {
    match decision {
        MkdirDecision::CreateReal => {
            fs::create_dir(path).map_err(Error::from)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(Error::from)?;
            Ok(())
        }
        MkdirDecision::CreateDaemon => {
            let meta = FileMetadata {
                size: 0,
                mode: libc::S_IFDIR | (mode & 0o7777),
                uid,
                gid,
                mtime: now_unix(),
                atime: now_unix(),
                ctime: now_unix(),
            };
            client
                .put_dir(path.to_path_buf(), meta)
                .map_err(Error::from)?;
            let _ = client.delete_whiteout(path.to_path_buf());
            Ok(())
        }
    }
}
