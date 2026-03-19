use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use dashmap::{DashMap, DashSet};
use fuser::{BackingId, FileAttr, FileType, INodeNo};

use crate::{
    error::{Error, Result},
    fuse::{
        attr::{attr_from_daemon, attr_from_meta},
        inode::InodeTable,
        AccessMode, OpenFile,
    },
    syncing::{proto::FuseEntry, EntryType, FileMetadata, PooledSyncClient, SyncClientPool},
};

pub(super) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn file_meta_with_now(size: u64, mode: u32, uid: u32, gid: u32) -> FileMetadata {
    let now = now_unix();
    FileMetadata {
        size,
        mode,
        uid,
        gid,
        mtime: now,
        atime: now,
        ctime: now,
    }
}

pub(super) fn unix_i64_to_u64_saturating(secs: i64) -> u64 {
    if secs.is_negative() {
        0
    } else {
        secs as u64
    }
}

pub(super) fn kind_from_entry(entry: &FuseEntry) -> Option<FileType> {
    match entry.entry_type {
        EntryType::Whiteout => None,
        EntryType::Dir => Some(FileType::Directory),
        EntryType::Symlink => Some(FileType::Symlink),
        EntryType::File => {
            let ft = entry.metadata.mode & libc::S_IFMT;
            if ft == libc::S_IFLNK {
                Some(FileType::Symlink)
            } else if ft == libc::S_IFDIR {
                Some(FileType::Directory)
            } else {
                Some(FileType::RegularFile)
            }
        }
    }
}

pub(super) fn normalize_abs(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        match comp {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if out != Path::new("/") {
                    out.pop();
                }
            }
            Component::Normal(name) => out.push(name),
            Component::Prefix(_) => {}
        }
    }
    out
}

pub struct Inner {
    pub(super) inodes: InodeTable,
    pub(super) open_files: DashMap<u64, OpenFile>,
    pub(super) backing_ids: DashMap<PathBuf, std::sync::Arc<BackingId>>,
    pub(super) next_fh: AtomicU64,
    pub(super) mount_uid: u32,
    pub(super) mount_gid: u32,
    pub(super) daemon_pool: SyncClientPool,
}

impl Inner {
    pub(super) fn new(daemon_sock: impl AsRef<Path>) -> Self {
        let inodes = InodeTable::new(PathBuf::from("/"));

        let mount_uid = nix::unistd::Uid::current().as_raw();
        let mount_gid = nix::unistd::Gid::current().as_raw();
        let daemon_pool = SyncClientPool::new(daemon_sock.as_ref().to_path_buf(), 12);

        Self {
            inodes,
            open_files: DashMap::new(),
            backing_ids: DashMap::new(),
            next_fh: AtomicU64::new(1),
            mount_uid,
            mount_gid,
            daemon_pool,
        }
    }

    pub(super) fn get_sync_client(&self) -> Result<PooledSyncClient> {
        self.daemon_pool.checkout().map_err(Error::from)
    }

    pub(super) fn path_of(&self, ino: INodeNo) -> Option<PathBuf> {
        self.inodes.get_path(ino.0)
    }

    pub(super) fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::AcqRel)
    }

    pub(super) fn resolve_ino(&self, ino: INodeNo) -> Option<(INodeNo, PathBuf)> {
        self.inodes.get_path(ino.0).map(|p| (ino, p))
    }

    pub(super) fn connect_daemon(&self) -> Result<PooledSyncClient> {
        self.daemon_pool.checkout().map_err(Error::from)
    }

    pub(super) fn stat_real_path(&self, path: &Path) -> Result<(FileType, FileAttr)> {
        let meta = fs::symlink_metadata(&path).map_err(Error::from)?;
        let kind = if meta.file_type().is_symlink() {
            FileType::Symlink
        } else if meta.file_type().is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let ino = self.inodes.get_or_insert(path);

        Ok((kind, attr_from_meta(ino, &meta)))
    }

    pub(super) fn stat_fuse_path(
        &self,
        path: &Path,
        client: &mut PooledSyncClient,
    ) -> Result<(FileType, FileAttr)> {
        match client.get_entry(path.to_path_buf()).map_err(Error::from)? {
            Some(entry) => {
                let kind = kind_from_entry(&entry)
                    .ok_or_else(|| Error::from(std::io::Error::from_raw_os_error(libc::ENOENT)))?;
                let ino = self.inodes.get_or_insert(path);
                Ok((kind, attr_from_daemon(ino, &entry.metadata, kind)))
            }
            None => Err(Error::from(std::io::Error::from_raw_os_error(libc::ENOENT))),
        }
    }

    pub(super) fn stat_path(
        &self,
        path: &Path,
        mode: &AccessMode,
        daemon: &mut PooledSyncClient,
    ) -> Result<(FileType, FileAttr)> {
        match mode {
            AccessMode::Passthrough => self.stat_real_path(path),
            AccessMode::FuseOnly => self.stat_fuse_path(path, daemon),
            AccessMode::CopyOnWrite => match self.stat_fuse_path(path, daemon) {
                Ok(v) => Ok(v),
                Err(Error::Io(e)) if e.raw_os_error() == Some(libc::ENOENT) => {
                    self.stat_real_path(path)
                }
                Err(err) => Err(err),
            },
        }
    }
}
