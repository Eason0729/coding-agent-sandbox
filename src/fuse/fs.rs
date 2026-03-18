use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::{DashMap, DashSet};

use fuser::{
    AccessFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, PollEvents, PollNotifier, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyLseek,
    ReplyOpen, ReplyPoll, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::fuse::attr::{attr_from_daemon, attr_from_meta};
use crate::fuse::inode::InodeTable;
use crate::fuse::open_file::{tmp_as_file, FileState, OpenFile, TTL};
use crate::fuse::policy::{AccessMode, Policy};
use crate::syncing::client::SyncClient;
use crate::syncing::pool::{PooledSyncClient, SyncClientPool};
use crate::syncing::proto::{EntryType, FileMetadata, FuseEntry};

macro_rules! get_sync_client {
    ($s:ident, $r: ident) => {
        match $s.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                $r.error(errno(code));
                return;
            }
        }
    };
}

fn errno(code: libc::c_int) -> Errno {
    Errno::from_i32(code)
}

fn io_errno(e: &std::io::Error) -> libc::c_int {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn file_meta_with_now(size: u64, mode: u32, uid: u32, gid: u32) -> FileMetadata {
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

fn unix_i64_to_u64_saturating(secs: i64) -> u64 {
    if secs.is_negative() {
        0
    } else {
        secs as u64
    }
}

fn kind_from_entry(entry: &FuseEntry) -> Option<FileType> {
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

fn normalize_abs(path: &Path) -> PathBuf {
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

struct Inner {
    root: PathBuf,
    inodes: InodeTable,
    open_files: DashMap<u64, OpenFile>,
    next_fh: AtomicU64,
    logged_once: DashSet<PathBuf>,
    mount_uid: u32,
    mount_gid: u32,
    daemon_pool: SyncClientPool,
}

impl Inner {
    fn path_of(&self, ino: INodeNo) -> Option<PathBuf> {
        self.inodes.get_path(ino.0)
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn real_path(&self, path: &Path) -> PathBuf {
        self.root
            .join(path.strip_prefix("/").unwrap_or(path))
            .to_path_buf()
    }

    fn resolve_ino(&self, ino: INodeNo) -> Option<(INodeNo, PathBuf)> {
        self.inodes.get_path(ino.0).map(|p| (ino, p))
    }

    fn connect_daemon(&self) -> Result<PooledSyncClient, libc::c_int> {
        self.daemon_pool.checkout().map_err(|_| libc::EIO)
    }

    fn stat_path(
        &self,
        path: &Path,
        mode: &AccessMode,
        daemon: &mut PooledSyncClient,
    ) -> Result<(FileType, FileAttr), libc::c_int> {
        match mode {
            AccessMode::Passthrough => {
                let real = self.real_path(path);
                let meta = fs::symlink_metadata(&real)
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
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
            AccessMode::CopyOnWrite => {
                if let Some(entry) = daemon
                    .get_entry(path.to_path_buf())
                    .map_err(|_| libc::EIO)?
                {
                    let kind = kind_from_entry(&entry).ok_or(libc::ENOENT)?;
                    let ino = self.inodes.get_or_insert(path);
                    let mut attr = attr_from_daemon(ino, &entry.metadata, kind);
                    if kind == FileType::Directory {
                        attr.uid = self.mount_uid;
                        attr.gid = self.mount_gid;
                    }
                    return Ok((kind, attr));
                }

                let real = self.real_path(path);
                let meta = fs::symlink_metadata(&real)
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
                let kind = if meta.file_type().is_symlink() {
                    FileType::Symlink
                } else if meta.file_type().is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let ino = self.inodes.get_or_insert(path);
                let mut attr = attr_from_meta(ino, &meta);
                if kind == FileType::Directory {
                    attr.uid = self.mount_uid;
                    attr.gid = self.mount_gid;
                }
                Ok((kind, attr))
            }
            AccessMode::FuseOnly => {
                let entry = daemon
                    .get_entry(path.to_path_buf())
                    .map_err(|_| libc::EIO)?
                    .ok_or(libc::ENOENT)?;
                let kind = kind_from_entry(&entry).ok_or(libc::ENOENT)?;
                let ino = self.inodes.get_or_insert(path);
                Ok((kind, attr_from_daemon(ino, &entry.metadata, kind)))
            }
        }
    }
}

pub struct CasFuseFs {
    inner: Arc<Inner>,
    policy: Arc<dyn Policy>,
}

impl CasFuseFs {
    pub fn new(
        root: PathBuf,
        daemon_sock: PathBuf,
        _daemon: SyncClient,
        policy: Arc<dyn Policy>,
        pool_size: usize,
    ) -> Self {
        let inodes = InodeTable::new(PathBuf::from("/"));
        let mount_uid = nix::unistd::Uid::current().as_raw();
        let mount_gid = nix::unistd::Gid::current().as_raw();
        Self {
            inner: Arc::new(Inner {
                root,
                inodes,
                open_files: DashMap::new(),
                next_fh: AtomicU64::new(1),
                logged_once: DashSet::new(),
                mount_uid,
                mount_gid,
                daemon_pool: SyncClientPool::new(daemon_sock, pool_size),
            }),
            policy,
        }
    }

    fn connect_daemon(&self) -> Result<PooledSyncClient, libc::c_int> {
        self.inner.daemon_pool.checkout().map_err(|_| libc::EIO)
    }

    fn lock(&self) -> Arc<Inner> {
        Arc::clone(&self.inner)
    }

    fn maybe_log(&self, g: &Inner, path: &Path, op: &str, pid: u32) {
        if !self.policy.should_log(path) {
            return;
        }
        if !g.logged_once.insert(path.to_path_buf()) {
            return;
        }
        if let Ok(mut daemon) = self.connect_daemon() {
            let _ = daemon.log_access(path.to_path_buf(), op.to_string(), pid);
        }
    }

    fn req_start(&self, req: &Request, op: &str, path: Option<&Path>, detail: &str) {
        let p = path
            .map(|v| v.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        log::debug!(
            "fuse.{op}.start pid={} uid={} gid={} path={} {}",
            req.pid(),
            req.uid(),
            req.gid(),
            p,
            detail
        );
    }

    fn req_ok(&self, req: &Request, op: &str, path: Option<&Path>, detail: &str) {
        let p = path
            .map(|v| v.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        log::debug!(
            "fuse.{op}.ok pid={} uid={} gid={} path={} {}",
            req.pid(),
            req.uid(),
            req.gid(),
            p,
            detail
        );
    }

    fn req_err(
        &self,
        req: &Request,
        op: &str,
        path: Option<&Path>,
        errno_code: libc::c_int,
        detail: &str,
    ) {
        let p = path
            .map(|v| v.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        log::debug!(
            "fuse.{op}.err pid={} uid={} gid={} errno={} path={} {}",
            req.pid(),
            req.uid(),
            req.gid(),
            errno_code,
            p,
            detail
        );
    }
}

impl Clone for CasFuseFs {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            policy: Arc::clone(&self.policy),
        }
    }
}

impl Filesystem for CasFuseFs {
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        const FLAGS: &[InitFlags] = &[
            InitFlags::FUSE_ASYNC_READ,
            InitFlags::FUSE_WRITEBACK_CACHE,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_WRITEBACK_CACHE,
            InitFlags::FUSE_BIG_WRITES,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ];
        for flag in FLAGS {
            config.add_capabilities(*flag).ok();
        }
        let caps = config.capabilities();
        log::debug!(
            "fuse.init pid={} uid={} gid={} caps={:?} atomic_o_trunc={} no_open_support={}",
            req.pid(),
            req.uid(),
            req.gid(),
            caps,
            caps.contains(InitFlags::FUSE_ATOMIC_O_TRUNC),
            caps.contains(InitFlags::FUSE_NO_OPEN_SUPPORT)
        );
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.req_start(
            req,
            "lookup",
            None,
            &format!("parent={} name={}", parent.0, name.to_string_lossy()),
        );
        let parent_path = match self.inner.path_of(parent) {
            Some(p) => p,
            None => {
                self.req_err(req, "lookup", None, libc::ENOENT, "parent inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        let mode = self.policy.classify(&path);
        let g = self.lock();

        let mut daemon = get_sync_client!(self, reply);
        self.maybe_log(&g, &path, "lookup", req.pid());
        match g.stat_path(&path, &mode, &mut daemon) {
            Ok((_kind, attr)) => {
                self.req_ok(req, "lookup", Some(&path), "entry found");
                reply.entry(&TTL, &attr, fuser::Generation(0));
            }
            Err(code) => {
                self.req_err(
                    req,
                    "lookup",
                    Some(&path),
                    code,
                    "entry missing or stat failed",
                );
                reply.error(errno(code));
            }
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.req_start(req, "getattr", None, &format!("ino={}", ino.0));
        let mut g = self.lock();
        let path = match g.path_of(ino) {
            Some(p) => p,
            None => {
                self.req_err(req, "getattr", None, libc::ENOENT, "inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        self.maybe_log(&g, &path, "getattr", req.pid());
        let mode = self.policy.classify(&path);
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                self.req_err(req, "getattr", Some(&path), code, "daemon connect failed");
                reply.error(errno(code));
                return;
            }
        };
        match g.stat_path(&path, &mode, &mut daemon) {
            Ok((_kind, attr)) => {
                self.req_ok(req, "getattr", Some(&path), "attr resolved");
                reply.attr(&TTL, &attr)
            }
            Err(code) => {
                self.req_err(req, "getattr", Some(&path), code, "stat failed");
                reply.error(errno(code));
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut g = self.lock();
        let path = match g.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let access = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);

                if let Some(new_mode) = mode {
                    match fs::symlink_metadata(&real) {
                        Ok(meta) => {
                            let ft = meta.mode() & libc::S_IFMT;
                            let perms = fs::Permissions::from_mode(ft | (new_mode & 0o7777));
                            if let Err(e) = fs::set_permissions(&real, perms) {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                        Err(e) => {
                            reply.error(errno(io_errno(&e)));
                            return;
                        }
                    }
                }

                if uid.is_some() || gid.is_some() {
                    let c_path = match std::ffi::CString::new(real.as_os_str().as_bytes()) {
                        Ok(v) => v,
                        Err(_) => {
                            reply.error(Errno::EINVAL);
                            return;
                        }
                    };
                    let chown_uid = uid.unwrap_or(u32::MAX);
                    let chown_gid = gid.unwrap_or(u32::MAX);
                    let rc = unsafe { libc::chown(c_path.as_ptr(), chown_uid, chown_gid) };
                    if rc != 0 {
                        reply.error(errno(
                            std::io::Error::last_os_error()
                                .raw_os_error()
                                .unwrap_or(libc::EIO),
                        ));
                        return;
                    }
                }

                if let Some(sz) = size {
                    let file = OpenOptions::new().write(true).open(&real);
                    let file = match file {
                        Ok(f) => f,
                        Err(e) => {
                            reply.error(errno(io_errno(&e)));
                            return;
                        }
                    };
                    if let Err(e) = file.set_len(sz) {
                        reply.error(errno(io_errno(&e)));
                        return;
                    }
                }

                let mut daemon = match g.connect_daemon() {
                    Ok(d) => d,
                    Err(code) => {
                        reply.error(errno(code));
                        return;
                    }
                };
                match g.stat_path(&path, &AccessMode::Passthrough, &mut daemon) {
                    Ok((_k, attr)) => reply.attr(&TTL, &attr),
                    Err(code) => reply.error(errno(code)),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                if let Some(fh) = fh {
                    if let Some(mut of) = g.open_files.get_mut(&fh.0) {
                        let root = g.root.clone();
                        if let Some(sz) = size {
                            match &mut of.state {
                                FileState::CowClean { .. } => {
                                    if let Err(code) = of.materialize(&root, &mut daemon) {
                                        reply.error(errno(code));
                                        return;
                                    }
                                    if let FileState::CowDirty { tmp, .. } = &mut of.state {
                                        if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                            reply.error(errno(io_errno(&e)));
                                            return;
                                        }
                                    }
                                }
                                FileState::CowDirty { tmp, .. }
                                | FileState::FuseOnlyDirty { tmp, .. }
                                | FileState::FuseOnlyNew { tmp } => {
                                    if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                        reply.error(errno(io_errno(&e)));
                                        return;
                                    }
                                }
                                FileState::FuseOnlyClean { .. } => {
                                    if let Err(code) = of.write_at(sz, &[], &root, &mut daemon) {
                                        reply.error(errno(code));
                                        return;
                                    }
                                    match &mut of.state {
                                        FileState::FuseOnlyDirty { tmp, .. } => {
                                            if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                                reply.error(errno(io_errno(&e)));
                                                return;
                                            }
                                        }
                                        FileState::FuseOnlyDirtyRanged { .. } => {
                                            of.set_ranged_size(sz);
                                        }
                                        _ => {}
                                    }
                                }
                                FileState::FuseOnlyDirtyRanged { .. } => {
                                    of.set_ranged_size(sz);
                                }
                                FileState::Passthrough { .. } => {}
                            }
                        }

                        if let Err(code) = of.flush_to_daemon(&mut daemon) {
                            reply.error(errno(code));
                            return;
                        }
                    }
                }

                let mut new_meta = if let Ok(Some(entry)) = daemon.get_entry(path.to_path_buf()) {
                    entry.metadata
                } else {
                    let real = g.real_path(&path);
                    match fs::symlink_metadata(&real) {
                        Ok(meta) => FileMetadata {
                            size: meta.size(),
                            mode: meta.mode(),
                            uid: meta.uid(),
                            gid: meta.gid(),
                            mtime: unix_i64_to_u64_saturating(meta.mtime()),
                            atime: unix_i64_to_u64_saturating(meta.atime()),
                            ctime: unix_i64_to_u64_saturating(meta.ctime()),
                        },
                        Err(_) => file_meta_with_now(0, libc::S_IFREG | 0o644, 0, 0),
                    }
                };

                if let Some(v) = mode {
                    let ft = new_meta.mode & libc::S_IFMT;
                    new_meta.mode = ft | (v & 0o7777);
                }
                if let Some(v) = uid {
                    new_meta.uid = v;
                }
                if let Some(v) = gid {
                    new_meta.gid = v;
                }
                if let Some(v) = size {
                    new_meta.size = v;
                }
                let now = now_unix();
                new_meta.mtime = now;
                new_meta.atime = now;
                new_meta.ctime = now;

                if daemon
                    .put_file_meta(path.to_path_buf(), new_meta.clone())
                    .is_err()
                {
                    let real = g.real_path(&path);
                    let mut bytes = fs::read(&real).unwrap_or_default();
                    if let Some(v) = size {
                        let new_len = v as usize;
                        if bytes.len() < new_len {
                            bytes.resize(new_len, 0);
                        } else {
                            bytes.truncate(new_len);
                        }
                    }
                    if daemon.put_file(path.clone(), bytes, new_meta).is_err() {
                        reply.error(Errno::EIO);
                        return;
                    }
                }

                match g.stat_path(&path, &access, &mut daemon) {
                    Ok((_k, attr)) => reply.attr(&TTL, &attr),
                    Err(code) => reply.error(errno(code)),
                }
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let mut g = self.lock();
        let path = match g.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let mode = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };

        let data = match mode {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::read_link(&real) {
                    Ok(target) => target.as_os_str().as_bytes().to_vec(),
                    Err(e) => {
                        reply.error(errno(io_errno(&e)));
                        return;
                    }
                }
            }
            AccessMode::FuseOnly => {
                let entry = match daemon.get_entry(path.clone()) {
                    Ok(Some(e)) => e,
                    Ok(None) => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                if kind_from_entry(&entry) != Some(FileType::Symlink) {
                    reply.error(Errno::EINVAL);
                    return;
                }
                match daemon.get_object(entry.id) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
            AccessMode::CopyOnWrite => {
                if let Ok(Some(entry)) = daemon.get_entry(path.clone()) {
                    if kind_from_entry(&entry) == Some(FileType::Symlink) {
                        match daemon.get_object(entry.id) {
                            Ok(bytes) => bytes,
                            Err(_) => {
                                reply.error(Errno::EIO);
                                return;
                            }
                        }
                    } else {
                        let real = g.real_path(&path);
                        match fs::read_link(&real) {
                            Ok(target) => target.as_os_str().as_bytes().to_vec(),
                            Err(e) => {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                    }
                } else {
                    let real = g.real_path(&path);
                    match fs::read_link(&real) {
                        Ok(target) => target.as_os_str().as_bytes().to_vec(),
                        Err(e) => {
                            reply.error(errno(io_errno(&e)));
                            return;
                        }
                    }
                }
            }
        };

        reply.data(&data);
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let mut g = self.lock();
        let parent_path = match g.path_of(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        let access = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        self.maybe_log(&g, &path, "mkdir", req.pid());

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::create_dir(&real) {
                    Ok(()) => {}
                    Err(e) => {
                        reply.error(errno(io_errno(&e)));
                        return;
                    }
                }

                let ino = g.inodes.get_or_insert(&path);
                match fs::symlink_metadata(&real) {
                    Ok(meta) => {
                        reply.entry(&TTL, &attr_from_meta(ino, &meta), fuser::Generation(0))
                    }
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let meta = crate::syncing::proto::DirMetadata {
                    mode: libc::S_IFDIR | (mode & 0o7777),
                    uid: req.uid(),
                    gid: req.gid(),
                    mtime: now_unix(),
                    atime: now_unix(),
                    ctime: now_unix(),
                };
                if daemon.put_dir(path.clone(), meta).is_err() {
                    reply.error(Errno::EIO);
                    return;
                }
                match g.stat_path(&path, &access, &mut daemon) {
                    Ok((_k, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
                    Err(code) => reply.error(errno(code)),
                }
            }
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.lock();
        let parent_path = match g.path_of(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = normalize_abs(&parent_path.join(name));
        let access = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        self.maybe_log(&g, &path, "unlink", req.pid());

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::remove_file(&real) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly => match daemon.delete_file(path) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
            AccessMode::CopyOnWrite => match daemon.get_entry(path.clone()) {
                Ok(Some(entry)) => {
                    if entry.entry_type == EntryType::Whiteout {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    match daemon.delete_file(path) {
                        Ok(()) => reply.ok(),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
                Ok(None) => {
                    let real = g.real_path(&path);
                    if real.exists() {
                        match daemon.put_whiteout(path) {
                            Ok(()) => reply.ok(),
                            Err(_) => reply.error(Errno::EIO),
                        }
                    } else {
                        reply.error(Errno::ENOENT);
                    }
                }
                Err(_) => reply.error(Errno::EIO),
            },
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.lock();
        let parent_path = match g.path_of(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = normalize_abs(&parent_path.join(name));
        let access = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        self.maybe_log(&g, &path, "rmdir", req.pid());

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::remove_dir(&real) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let children = match daemon.read_dir_all(path.clone()) {
                    Ok(v) => v,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                if !children.is_empty() {
                    reply.error(Errno::ENOTEMPTY);
                    return;
                }

                if matches!(access, AccessMode::CopyOnWrite)
                    && matches!(daemon.get_entry(path.clone()), Ok(None))
                {
                    let real = g.real_path(&path);
                    if real.exists() {
                        match daemon.put_whiteout(path) {
                            Ok(()) => reply.ok(),
                            Err(_) => reply.error(Errno::EIO),
                        }
                    } else {
                        reply.error(Errno::ENOENT)
                    }
                } else {
                    match daemon.delete_file(path) {
                        Ok(()) => reply.ok(),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
            }
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut g = self.lock();
        let from_parent = match g.path_of(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let to_parent = match g.path_of(newparent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let from = normalize_abs(&from_parent.join(name));
        let to = normalize_abs(&to_parent.join(newname));

        let from_mode = self.policy.classify(&from);
        let to_mode = self.policy.classify(&to);
        if from_mode != to_mode {
            reply.error(Errno::EPERM);
            return;
        }

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        self.maybe_log(&g, &from, "rename", req.pid());

        match from_mode {
            AccessMode::Passthrough => {
                let real_from = g.real_path(&from);
                let real_to = g.real_path(&to);
                match fs::rename(&real_from, &real_to) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => match daemon.rename_file(from, to) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let mut g = self.lock();
        let parent_path = match g.path_of(parent) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        let access = self.policy.classify(&path);

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        self.maybe_log(&g, &path, "symlink", req.pid());

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match std::os::unix::fs::symlink(target, &real) {
                    Ok(()) => {
                        let ino = g.inodes.get_or_insert(&path);
                        match fs::symlink_metadata(&real) {
                            Ok(meta) => {
                                reply.entry(&TTL, &attr_from_meta(ino, &meta), fuser::Generation(0))
                            }
                            Err(e) => reply.error(errno(io_errno(&e))),
                        }
                    }
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let bytes = target.as_os_str().as_bytes().to_vec();
                let meta = file_meta_with_now(
                    bytes.len() as u64,
                    libc::S_IFLNK | 0o777,
                    req.uid(),
                    req.gid(),
                );
                if daemon.put_file(path.clone(), bytes, meta).is_err() {
                    reply.error(Errno::EIO);
                    return;
                }
                match g.stat_path(&path, &access, &mut daemon) {
                    Ok((_k, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
                    Err(code) => reply.error(errno(code)),
                }
            }
        }
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.req_start(
            req,
            "open",
            None,
            &format!("ino={} flags={}", ino.0, flags.0),
        );
        let mut g = self.lock();
        let (_, path) = match g.resolve_ino(ino) {
            Some(v) => v,
            None => {
                self.req_err(req, "open", None, libc::ENOENT, "inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        self.maybe_log(&g, &path, "open", req.pid());
        let access = self.policy.classify(&path);
        let accmode = flags.0 & libc::O_ACCMODE;
        let wants_write = accmode != libc::O_RDONLY;
        let trunc = (flags.0 & libc::O_TRUNC) != 0;

        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                self.req_err(req, "open", Some(&path), code, "daemon connect failed");
                reply.error(errno(code));
                return;
            }
        };

        let state = match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                let mut opts = OpenOptions::new();
                opts.read(accmode != libc::O_WRONLY)
                    .write(accmode != libc::O_RDONLY)
                    .append((flags.0 & libc::O_APPEND) != 0);
                match opts.open(&real) {
                    Ok(file) => FileState::Passthrough { file },
                    Err(e) => {
                        reply.error(errno(io_errno(&e)));
                        return;
                    }
                }
            }
            AccessMode::FuseOnly => {
                let entry = match daemon.get_entry(path.clone()) {
                    Ok(Some(e)) => e,
                    Ok(None) => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                match entry.entry_type {
                    EntryType::Whiteout => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    EntryType::Dir => {
                        reply.error(Errno::EISDIR);
                        return;
                    }
                    _ => {}
                }
                if trunc && wants_write {
                    let mut tmp = match tempfile::NamedTempFile::new() {
                        Ok(t) => t,
                        Err(_) => {
                            reply.error(Errno::EIO);
                            return;
                        }
                    };
                    if tmp.as_file_mut().set_len(0).is_err() {
                        reply.error(Errno::EIO);
                        return;
                    }
                    FileState::FuseOnlyDirty {
                        tmp,
                        object_id: entry.id,
                    }
                } else {
                    FileState::FuseOnlyClean {
                        object_id: entry.id,
                    }
                }
            }
            AccessMode::CopyOnWrite => {
                let daemon_entry = match daemon.get_entry(path.clone()) {
                    Ok(v) => v,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                if let Some(entry) = daemon_entry {
                    if entry.entry_type == EntryType::Whiteout {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    if kind_from_entry(&entry) == Some(FileType::Directory) {
                        reply.error(Errno::EISDIR);
                        return;
                    }
                    if trunc && wants_write {
                        let mut tmp = match tempfile::NamedTempFile::new() {
                            Ok(t) => t,
                            Err(_) => {
                                reply.error(Errno::EIO);
                                return;
                            }
                        };
                        if tmp.as_file_mut().set_len(0).is_err() {
                            reply.error(Errno::EIO);
                            return;
                        }
                        FileState::CowDirty {
                            tmp,
                            object_id_before: Some(entry.id),
                        }
                    } else {
                        FileState::CowClean {
                            object_id: Some(entry.id),
                        }
                    }
                } else {
                    let real = g.real_path(&path);
                    if !real.exists() {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    if trunc && wants_write {
                        let mut tmp = match tempfile::NamedTempFile::new() {
                            Ok(t) => t,
                            Err(_) => {
                                reply.error(Errno::EIO);
                                return;
                            }
                        };
                        if tmp.as_file_mut().set_len(0).is_err() {
                            reply.error(Errno::EIO);
                            return;
                        }
                        FileState::CowDirty {
                            tmp,
                            object_id_before: None,
                        }
                    } else {
                        FileState::CowClean { object_id: None }
                    }
                }
            }
        };

        let fh = g.alloc_fh();
        g.open_files.insert(
            fh,
            OpenFile {
                path: path.clone(),
                state,
            },
        );

        self.req_ok(req, "open", Some(&path), &format!("fh={fh}"));
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.req_start(
            req,
            "create",
            None,
            &format!(
                "parent={} name={} mode={:o} flags={}",
                parent.0,
                name.to_string_lossy(),
                mode,
                flags
            ),
        );
        let mut g = self.lock();
        let parent_path = match g.path_of(parent) {
            Some(p) => p,
            None => {
                self.req_err(req, "create", None, libc::ENOENT, "parent inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(&g, &path, "create", req.pid());
        let access = self.policy.classify(&path);
        let mut daemon = match g.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                self.req_err(req, "create", Some(&path), code, "daemon connect failed");
                reply.error(errno(code));
                return;
            }
        };

        let (attr, of_state) = match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                let file = OpenOptions::new()
                    .write(true)
                    .read(true)
                    .create(true)
                    .truncate(true)
                    .mode(mode)
                    .open(&real);
                let file = match file {
                    Ok(f) => f,
                    Err(e) => {
                        let code = io_errno(&e);
                        self.req_err(req, "create", Some(&path), code, "passthrough open failed");
                        reply.error(errno(code));
                        return;
                    }
                };
                let ino = g.inodes.get_or_insert(&path);
                let meta = match fs::symlink_metadata(&real) {
                    Ok(m) => m,
                    Err(e) => {
                        let code = io_errno(&e);
                        self.req_err(req, "create", Some(&path), code, "passthrough stat failed");
                        reply.error(errno(code));
                        return;
                    }
                };
                (attr_from_meta(ino, &meta), FileState::Passthrough { file })
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let meta =
                    file_meta_with_now(0, libc::S_IFREG | (mode & 0o7777), req.uid(), req.gid());
                if daemon
                    .put_file(path.clone(), Vec::new(), meta.clone())
                    .is_err()
                {
                    self.req_err(
                        req,
                        "create",
                        Some(&path),
                        libc::EIO,
                        "daemon put_file failed",
                    );
                    reply.error(Errno::EIO);
                    return;
                }
                let ino = g.inodes.get_or_insert(&path);
                (
                    attr_from_daemon(ino, &meta, FileType::RegularFile),
                    FileState::FuseOnlyClean { object_id: 0 },
                )
            }
        };

        let fh = g.alloc_fh();
        let state = match access {
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let obj = match daemon.get_entry(path.clone()) {
                    Ok(Some(entry)) => entry.id,
                    _ => 0,
                };
                let _ = of_state;
                FileState::FuseOnlyClean { object_id: obj }
            }
            AccessMode::Passthrough => of_state,
        };

        g.open_files.insert(
            fh,
            OpenFile {
                path: path.clone(),
                state,
            },
        );

        let open_flags = FopenFlags::from_bits(flags as u32).unwrap_or(FopenFlags::empty());
        self.req_ok(req, "create", Some(&path), &format!("fh={fh}"));
        reply.created(
            &TTL,
            &attr,
            fuser::Generation(0),
            FileHandle(fh),
            open_flags,
        );
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };

        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = g.root.clone();
        let res = match g.open_files.get_mut(&fh.0) {
            Some(mut of) => of.read_at(offset, size, &root, &mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        match res {
            Ok(buf) => reply.data(&buf),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };

        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = g.root.clone();
        let res = match g.open_files.get_mut(&fh.0) {
            Some(mut of) => of.write_at(offset, data, &root, &mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        match res {
            Ok(n) => reply.written(n as u32),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        let g = self.lock();
        if g.path_of(ino_in).is_none() || g.path_of(ino_out).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = g.root.clone();

        if fh_in.0 == fh_out.0 {
            let res = match g.open_files.get_mut(&fh_in.0) {
                Some(mut of) => match of.copy_from(offset_in, len, &root, &mut daemon) {
                    Ok(data) => of.write_at(offset_out, &data, &root, &mut daemon),
                    Err(code) => Err(code),
                },
                None => {
                    reply.error(Errno::EBADF);
                    return;
                }
            };
            match res {
                Ok(n) => reply.written(n as u32),
                Err(code) => reply.error(errno(code)),
            }
            return;
        }

        let data = match g.open_files.get_mut(&fh_in.0) {
            Some(mut of) => match of.copy_from(offset_in, len, &root, &mut daemon) {
                Ok(v) => v,
                Err(code) => {
                    reply.error(errno(code));
                    return;
                }
            },
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let write_res = match g.open_files.get_mut(&fh_out.0) {
            Some(mut of) => of.write_at(offset_out, &data, &root, &mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        match write_res {
            Ok(n) => reply.written(n as u32),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        let g = self.lock();
        let res = match g.open_files.get_mut(&fh.0) {
            Some(mut of) => of.flush_to_daemon(&mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        match res {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let g = self.lock();
        let mut of = match g.open_files.remove(&fh.0) {
            Some((_k, v)) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        if flush {
            let mut daemon = match self.connect_daemon() {
                Ok(d) => d,
                Err(code) => {
                    reply.error(errno(code));
                    return;
                }
            };
            if let Err(code) = of.flush_to_daemon(&mut daemon) {
                reply.error(errno(code));
                return;
            }
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        let g = self.lock();
        let res = match g.open_files.get_mut(&fh.0) {
            Some(mut of) => of.flush_to_daemon(&mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        match res {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        use std::io::{Seek, SeekFrom};

        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = g.root.clone();

        let mut of = match g.open_files.get_mut(&fh.0) {
            Some(of) => of,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        let result: Result<u64, std::io::Error> = match &mut of.state {
            FileState::Passthrough { file } => {
                let seek_from = match whence {
                    0 => SeekFrom::Start(offset as u64),
                    1 => SeekFrom::Current(offset),
                    2 => SeekFrom::End(offset),
                    _ => {
                        reply.error(Errno::EINVAL);
                        return;
                    }
                };
                file.seek(seek_from).map(|pos| pos as u64)
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                let mut f = tmp_as_file(tmp);
                let seek_from = match whence {
                    0 => SeekFrom::Start(offset as u64),
                    1 => SeekFrom::Current(offset),
                    2 => SeekFrom::End(offset),
                    _ => {
                        reply.error(Errno::EINVAL);
                        return;
                    }
                };
                f.seek(seek_from).map(|pos| pos as u64)
            }
            FileState::CowClean { object_id } => {
                let size = if let Some(id) = object_id {
                    match daemon.get_object(*id) {
                        Ok(bytes) => bytes.len() as u64,
                        Err(_) => {
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                } else {
                    let real_path = root.join(of.path.strip_prefix("/").unwrap_or(&of.path));
                    match std::fs::metadata(&real_path) {
                        Ok(meta) => meta.len(),
                        Err(_) => {
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                };
                if whence == 1 {
                    let mut mat = false;
                    if let Ok(Some(entry)) = daemon.get_entry(of.path.clone()) {
                        if entry.metadata.size > 0 {
                            mat = true;
                        }
                    }
                    if mat {
                        if let Err(code) = of.materialize(&root, &mut daemon) {
                            reply.error(errno(code));
                            return;
                        }
                        let seek_from = SeekFrom::Current(offset);
                        let pos = if let FileState::CowDirty { tmp, .. } = &mut of.state {
                            let mut f = tmp_as_file(tmp);
                            f.seek(seek_from)
                        } else {
                            Err(std::io::Error::from_raw_os_error(libc::EIO))
                        };
                        match pos {
                            Ok(p) => {
                                reply.offset(p as i64);
                                return;
                            }
                            Err(e) => {
                                reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
                                return;
                            }
                        }
                    }
                }
                let new_pos = match whence {
                    0 => offset as u64,
                    1 => {
                        reply.error(Errno::EPERM);
                        return;
                    }
                    2 => (size as i64).saturating_add(offset) as u64,
                    _ => {
                        reply.error(Errno::EINVAL);
                        return;
                    }
                };
                Ok(new_pos)
            }
            FileState::FuseOnlyClean { object_id } => {
                let _id = *object_id;
                let size = match daemon.get_entry(of.path.clone()) {
                    Ok(Some(entry)) => entry.metadata.size,
                    Ok(None) => 0,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                if whence == 1 {
                    if let Err(code) = of.write_at(0, &[], &root, &mut daemon) {
                        reply.error(errno(code));
                        return;
                    }
                    if let FileState::FuseOnlyDirtyRanged {
                        logical_size,
                        truncate_to,
                        ..
                    } = &mut of.state
                    {
                        let current = truncate_to.unwrap_or(*logical_size);
                        let new_pos = (current as i64).saturating_add(offset).max(0) as u64;
                        *logical_size = new_pos;
                        *truncate_to = Some(new_pos);
                        reply.offset(new_pos as i64);
                        return;
                    }
                }
                let new_pos = match whence {
                    0 => offset as u64,
                    1 => {
                        reply.error(Errno::EPERM);
                        return;
                    }
                    2 => (size as i64).saturating_add(offset) as u64,
                    _ => {
                        reply.error(Errno::EINVAL);
                        return;
                    }
                };
                Ok(new_pos)
            }
            FileState::FuseOnlyDirtyRanged {
                logical_size,
                truncate_to,
                ..
            } => {
                let size = truncate_to.unwrap_or(*logical_size);
                let new_pos = match whence {
                    0 => offset as u64,
                    1 => (size as i64).saturating_add(offset).max(0) as u64,
                    2 => (size as i64).saturating_add(offset) as u64,
                    _ => {
                        reply.error(Errno::EINVAL);
                        return;
                    }
                };
                *logical_size = new_pos;
                *truncate_to = Some(new_pos);
                Ok(new_pos)
            }
        };

        match result {
            Ok(pos) => reply.offset(pos as i64),
            Err(e) => reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        reply.opened(FileHandle(ino.0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut g = self.lock();
        let path = match g.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let access = self.policy.classify(&path);
        let mut merged: BTreeMap<Vec<u8>, (PathBuf, FileType)> = BTreeMap::new();
        let mut daemon = if !matches!(access, AccessMode::Passthrough) {
            match g.connect_daemon() {
                Ok(d) => Some(d),
                Err(code) => {
                    reply.error(errno(code));
                    return;
                }
            }
        } else {
            None
        };

        if !matches!(access, AccessMode::FuseOnly) {
            let real = g.real_path(&path);
            if let Ok(dir) = fs::read_dir(&real) {
                for entry in dir.flatten() {
                    let name = entry.file_name();
                    let full = normalize_abs(&path.join(&name));
                    if let Ok(meta) = entry.metadata() {
                        let kind = if meta.file_type().is_dir() {
                            FileType::Directory
                        } else if meta.file_type().is_symlink() {
                            FileType::Symlink
                        } else {
                            FileType::RegularFile
                        };
                        merged.insert(name.as_bytes().to_vec(), (full, kind));
                    }
                }
            }
        }

        if !matches!(access, AccessMode::Passthrough) {
            if let Some(daemon) = daemon.as_mut() {
                if let Ok(entries) = daemon.read_dir_all(path.clone()) {
                    for (child_path, entry) in entries {
                        let Some(name) = child_path.file_name() else {
                            continue;
                        };
                        if entry.entry_type == EntryType::Whiteout {
                            merged.remove(name.as_bytes());
                            continue;
                        }
                        if let Some(kind) = kind_from_entry(&entry) {
                            merged.insert(name.as_bytes().to_vec(), (child_path, kind));
                        }
                    }
                }
            }
        }

        let parent_path = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let parent_ino = INodeNo(g.inodes.get_or_insert(&parent_path));

        let mut entries: Vec<(INodeNo, FileType, Vec<u8>)> = Vec::new();
        entries.push((ino, FileType::Directory, b".".to_vec()));
        entries.push((parent_ino, FileType::Directory, b"..".to_vec()));

        for (_name, (child_path, kind)) in merged {
            let child_ino = INodeNo(g.inodes.get_or_insert(&child_path));
            let child_name = child_path
                .file_name()
                .map(OsStr::as_bytes)
                .unwrap_or_default()
                .to_vec();
            entries.push((child_ino, kind, child_name));
        }

        for (idx, (entry_ino, kind, name)) in entries.into_iter().enumerate() {
            if (idx as u64) < offset {
                continue;
            }
            if reply.add(entry_ino, (idx + 1) as u64, kind, OsStr::from_bytes(&name)) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let mut g = self.lock();
        let path = match g.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let access = self.policy.classify(&path);
        let mut merged: BTreeMap<Vec<u8>, (PathBuf, FileType)> = BTreeMap::new();
        let mut daemon = if !matches!(access, AccessMode::Passthrough) {
            match g.connect_daemon() {
                Ok(d) => Some(d),
                Err(code) => {
                    reply.error(errno(code));
                    return;
                }
            }
        } else {
            None
        };

        if !matches!(access, AccessMode::FuseOnly) {
            let real = g.real_path(&path);
            if let Ok(dir) = fs::read_dir(&real) {
                for entry in dir.flatten() {
                    let name = entry.file_name();
                    let full = normalize_abs(&path.join(&name));
                    if let Ok(meta) = entry.metadata() {
                        let kind = if meta.file_type().is_dir() {
                            FileType::Directory
                        } else if meta.file_type().is_symlink() {
                            FileType::Symlink
                        } else {
                            FileType::RegularFile
                        };
                        merged.insert(name.as_bytes().to_vec(), (full, kind));
                    }
                }
            }
        }

        if !matches!(access, AccessMode::Passthrough) {
            if let Some(daemon) = daemon.as_mut() {
                if let Ok(entries) = daemon.read_dir_all(path.clone()) {
                    for (child_path, entry) in entries {
                        let Some(name) = child_path.file_name() else {
                            continue;
                        };
                        if entry.entry_type == EntryType::Whiteout {
                            merged.remove(name.as_bytes());
                            continue;
                        }
                        if let Some(kind) = kind_from_entry(&entry) {
                            merged.insert(name.as_bytes().to_vec(), (child_path, kind));
                        }
                    }
                }
            }
        }

        let parent_path = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let parent_ino = INodeNo(g.inodes.get_or_insert(&parent_path));

        let mut entries: Vec<(INodeNo, FileType, Vec<u8>)> = Vec::new();
        entries.push((ino, FileType::Directory, b".".to_vec()));
        entries.push((parent_ino, FileType::Directory, b"..".to_vec()));

        for (_name, (child_path, kind)) in merged {
            let child_ino = INodeNo(g.inodes.get_or_insert(&child_path));
            let child_name = child_path
                .file_name()
                .map(OsStr::as_bytes)
                .unwrap_or_default()
                .to_vec();
            entries.push((child_ino, kind, child_name));
        }

        for (idx, (entry_ino, kind, name)) in entries.into_iter().enumerate() {
            if (idx as u64) < offset {
                continue;
            }
            let child_path = if name == b"." {
                path.clone()
            } else if name == b".." {
                parent_path.clone()
            } else {
                path.join(OsStr::from_bytes(&name))
            };
            let mode = self.policy.classify(&child_path);
            let attr = if let Some(daemon) = daemon.as_mut() {
                match g.stat_path(&child_path, &mode, daemon) {
                    Ok((_, a)) => a,
                    Err(_) => continue,
                }
            } else {
                let real = g.real_path(&child_path);
                match fs::symlink_metadata(&real) {
                    Ok(meta) => attr_from_meta(g.inodes.get_or_insert(&child_path), &meta),
                    Err(_) => continue,
                }
            };
            if reply.add(
                entry_ino,
                (idx + 1) as u64,
                OsStr::from_bytes(&name),
                &TTL,
                &attr,
                fuser::Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let g = self.lock();
        match nix::sys::statvfs::statvfs(g.root.as_path()) {
            Ok(stat) => {
                reply.statfs(
                    stat.blocks() as u64,
                    stat.blocks_free() as u64,
                    stat.blocks_available() as u64,
                    stat.files() as u64,
                    stat.files_free() as u64,
                    stat.block_size() as u32,
                    stat.name_max() as u32,
                    stat.fragment_size() as u32,
                );
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        log::debug!("fuse.access.start ino={} mask={:?}", ino.0, _mask);
        let g = self.lock();
        if g.path_of(ino).is_some() {
            log::debug!("fuse.access.ok ino={}", ino.0);
            reply.ok();
        } else {
            log::debug!("fuse.access.err ino={} errno={}", ino.0, libc::ENOENT);
            reply.error(Errno::ENOENT);
        }
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOTSUP);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }

    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        // tcl use it: /usr/bin/tclsh9.0

        reply.error(Errno::ENOTSUP);
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let mut daemon = match self.connect_daemon() {
            Ok(d) => d,
            Err(code) => {
                reply.error(errno(code));
                return;
            }
        };
        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = g.root.clone();
        let mut of = match g.open_files.get_mut(&fh.0) {
            Some(of) => of,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        let of_path = of.path.clone();
        let res = match &mut of.state {
            FileState::Passthrough { file } => {
                let fd = file.as_raw_fd();
                let ret = unsafe {
                    libc::fallocate(fd, mode, offset as libc::off_t, length as libc::off_t)
                };
                if ret == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO))
                }
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = tmp_as_file(tmp);
                let end = offset.saturating_add(length);
                let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);
                if end > current_size {
                    let mode_keep_size = mode & libc::FALLOC_FL_KEEP_SIZE as i32;
                    if mode_keep_size == 0 {
                        match file.seek(SeekFrom::Start(end)) {
                            Ok(_) => {}
                            Err(e) => {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                        match file.write_all(&[0]) {
                            Ok(_) => {}
                            Err(e) => {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                    } else {
                        let diff = end - current_size;
                        match file.seek(SeekFrom::Start(current_size)) {
                            Ok(_) => {}
                            Err(e) => {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                        let zeros = vec![0u8; diff as usize];
                        match file.write_all(&zeros) {
                            Ok(_) => {}
                            Err(e) => {
                                reply.error(errno(io_errno(&e)));
                                return;
                            }
                        }
                    }
                }
                Ok(())
            }
            FileState::CowClean {
                object_id: object_id_opt,
            } => {
                let size = if let Some(id) = *object_id_opt {
                    match daemon.get_object(id) {
                        Ok(bytes) => bytes.len() as u64,
                        Err(_) => {
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                } else {
                    let real_path = root.join(of_path.strip_prefix("/").unwrap_or(&of_path));
                    match std::fs::metadata(&real_path) {
                        Ok(meta) => meta.len(),
                        Err(_) => {
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                };
                let end = offset.saturating_add(length);
                if end > size {
                    let mode_keep_size = mode & libc::FALLOC_FL_KEEP_SIZE as i32;
                    if mode_keep_size != 0 {
                        if let Some(id) = *object_id_opt {
                            if let Ok(mut bytes) = daemon.get_object(id) {
                                bytes.resize(end as usize, 0);
                                let _ = daemon.put_file(
                                    of_path.clone(),
                                    bytes.clone(),
                                    crate::syncing::proto::FileMetadata {
                                        size: bytes.len() as u64,
                                        mode: libc::S_IFREG | 0o644,
                                        uid: 0,
                                        gid: 0,
                                        mtime: now_unix(),
                                        atime: now_unix(),
                                        ctime: now_unix(),
                                    },
                                );
                            }
                        }
                    }
                }
                Ok(())
            }
            FileState::FuseOnlyClean { object_id } => {
                let size = match daemon.get_object(*object_id) {
                    Ok(bytes) => bytes.len() as u64,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                let end = offset.saturating_add(length);
                if end > size {
                    let mode_keep_size = mode & libc::FALLOC_FL_KEEP_SIZE as i32;
                    if mode_keep_size != 0 {
                        if let Ok(mut bytes) = daemon.get_object(*object_id) {
                            bytes.resize(end as usize, 0);
                            let _ = daemon.put_file(
                                of_path.clone(),
                                bytes.clone(),
                                crate::syncing::proto::FileMetadata {
                                    size: bytes.len() as u64,
                                    mode: libc::S_IFREG | 0o644,
                                    uid: 0,
                                    gid: 0,
                                    mtime: now_unix(),
                                    atime: now_unix(),
                                    ctime: now_unix(),
                                },
                            );
                        }
                    }
                }
                Ok(())
            }
            FileState::FuseOnlyDirtyRanged {
                logical_size,
                truncate_to,
                ..
            } => {
                let end = offset.saturating_add(length);
                let mode_keep_size = mode & libc::FALLOC_FL_KEEP_SIZE as i32;
                if mode_keep_size == 0 {
                    *logical_size = (*logical_size).max(end);
                    *truncate_to = Some(truncate_to.unwrap_or(*logical_size).max(end));
                }
                Ok(())
            }
        };

        match res {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
        }
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn poll(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _ph: PollNotifier,
        events: PollEvents,
        _flags: fuser::PollFlags,
        reply: ReplyPoll,
    ) {
        let g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }

        let mut of = match g.open_files.get_mut(&fh.0) {
            Some(of) => of,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let revents = match &mut of.state {
            FileState::Passthrough { file } => {
                let fd = file.as_raw_fd();
                let mut pollfd = libc::pollfd {
                    fd,
                    events: events.bits() as i16,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pollfd, 1, 0) };
                if ret < 0 {
                    reply.error(errno(libc::EIO));
                    return;
                }
                PollEvents::from_bits_truncate(pollfd.revents as u32)
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                let file = tmp_as_file(tmp);
                let fd = file.as_raw_fd();
                let mut pollfd = libc::pollfd {
                    fd,
                    events: events.bits() as i16,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pollfd, 1, 0) };
                if ret < 0 {
                    reply.error(errno(libc::EIO));
                    return;
                }
                PollEvents::from_bits_truncate(pollfd.revents as u32)
            }
            FileState::CowClean { .. }
            | FileState::FuseOnlyClean { .. }
            | FileState::FuseOnlyDirtyRanged { .. } => PollEvents::POLLIN | PollEvents::POLLOUT,
        };

        reply.poll(revents);
    }
}
