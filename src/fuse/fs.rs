use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};

use crate::fuse::attr::{attr_from_daemon, attr_from_meta};
use crate::fuse::inode::InodeTable;
use crate::fuse::open_file::{FileState, OpenFile, TTL};
use crate::fuse::policy::{AccessMode, Policy};
use crate::syncing::client::SyncClient;
use crate::syncing::proto::{EntryType, FileMetadata, FuseEntry};

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
    open_files: HashMap<u64, OpenFile>,
    next_fh: u64,
    daemon: SyncClient,
    logged_once: HashSet<PathBuf>,
    mount_uid: u32,
    mount_gid: u32,
}

impl Inner {
    fn path_of(&self, ino: INodeNo) -> Option<&Path> {
        self.inodes.get_path(ino.0)
    }

    fn alloc_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh = self.next_fh.saturating_add(1);
        fh
    }

    fn real_path(&self, path: &Path) -> PathBuf {
        self.root
            .join(path.strip_prefix("/").unwrap_or(path))
            .to_path_buf()
    }

    fn resolve_ino(&self, ino: INodeNo) -> Option<(INodeNo, PathBuf)> {
        self.inodes.get_path(ino.0).map(|p| (ino, p.to_path_buf()))
    }

    fn stat_path(
        &mut self,
        path: &Path,
        mode: &AccessMode,
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
                if let Some(entry) = self
                    .daemon
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
                let entry = self
                    .daemon
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
    inner: Arc<Mutex<Inner>>,
    policy: Arc<dyn Policy>,
}

impl CasFuseFs {
    pub fn new(root: PathBuf, daemon: SyncClient, policy: Arc<dyn Policy>) -> Self {
        let inodes = InodeTable::new(PathBuf::from("/"));
        let mount_uid = nix::unistd::Uid::current().as_raw();
        let mount_gid = nix::unistd::Gid::current().as_raw();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                root,
                inodes,
                open_files: HashMap::new(),
                next_fh: 1,
                daemon,
                logged_once: HashSet::new(),
                mount_uid,
                mount_gid,
            })),
            policy,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("CasFuseFs mutex poisoned")
    }

    fn maybe_log(&self, req: &Request, g: &mut Inner, path: &Path, op: &str) {
        if !self.policy.should_log(path) {
            return;
        }
        if !g.logged_once.insert(path.to_path_buf()) {
            return;
        }
        let _ = g
            .daemon
            .log_access(path.to_path_buf(), op.to_string(), req.pid());
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
        let mut g = self.lock();
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                self.req_err(req, "lookup", None, libc::ENOENT, "parent inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "lookup");
        let mode = self.policy.classify(&path);

        match g.stat_path(&path, &mode) {
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
        let path = match g.path_of(ino).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                self.req_err(req, "getattr", None, libc::ENOENT, "inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        self.maybe_log(req, &mut g, &path, "getattr");
        let mode = self.policy.classify(&path);
        match g.stat_path(&path, &mode) {
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
        let path = match g.path_of(ino).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let access = self.policy.classify(&path);

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

                match g.stat_path(&path, &AccessMode::Passthrough) {
                    Ok((_k, attr)) => reply.attr(&TTL, &attr),
                    Err(code) => reply.error(errno(code)),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                if let Some(fh) = fh {
                    if let Some(mut of) = g.open_files.remove(&fh.0) {
                        let root = g.root.clone();
                        if let Some(sz) = size {
                            match &mut of.state {
                                FileState::CowClean { .. } => {
                                    if let Err(code) = of.materialize(&root, &mut g.daemon) {
                                        g.open_files.insert(fh.0, of);
                                        reply.error(errno(code));
                                        return;
                                    }
                                    if let FileState::CowDirty { tmp, .. } = &mut of.state {
                                        if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                            g.open_files.insert(fh.0, of);
                                            reply.error(errno(io_errno(&e)));
                                            return;
                                        }
                                    }
                                }
                                FileState::CowDirty { tmp, .. }
                                | FileState::FuseOnlyDirty { tmp, .. }
                                | FileState::FuseOnlyNew { tmp } => {
                                    if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                        g.open_files.insert(fh.0, of);
                                        reply.error(errno(io_errno(&e)));
                                        return;
                                    }
                                }
                                FileState::FuseOnlyClean { .. } => {
                                    if let Err(code) = of.write_at(sz, &[], &root, &mut g.daemon) {
                                        g.open_files.insert(fh.0, of);
                                        reply.error(errno(code));
                                        return;
                                    }
                                    if let FileState::FuseOnlyDirty { tmp, .. } = &mut of.state {
                                        if let Err(e) = tmp.as_file_mut().set_len(sz) {
                                            g.open_files.insert(fh.0, of);
                                            reply.error(errno(io_errno(&e)));
                                            return;
                                        }
                                    }
                                }
                                FileState::Passthrough { .. } => {}
                            }
                        }

                        if let Err(code) = of.flush_to_daemon(&mut g.daemon) {
                            g.open_files.insert(fh.0, of);
                            reply.error(errno(code));
                            return;
                        }

                        g.open_files.insert(fh.0, of);
                    }
                }

                let mut new_meta = if let Ok(Some(entry)) = g.daemon.get_entry(path.to_path_buf()) {
                    entry.metadata
                } else {
                    let real = g.real_path(&path);
                    match fs::symlink_metadata(&real) {
                        Ok(meta) => FileMetadata {
                            size: meta.size(),
                            mode: meta.mode(),
                            uid: meta.uid(),
                            gid: meta.gid(),
                            mtime: meta.mtime() as u64,
                            atime: meta.atime() as u64,
                            ctime: meta.ctime() as u64,
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

                if g.daemon
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
                    if g.daemon.put_file(path.clone(), bytes, new_meta).is_err() {
                        reply.error(Errno::EIO);
                        return;
                    }
                }

                match g.stat_path(&path, &access) {
                    Ok((_k, attr)) => reply.attr(&TTL, &attr),
                    Err(code) => reply.error(errno(code)),
                }
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let mut g = self.lock();
        let path = match g.path_of(ino).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let mode = self.policy.classify(&path);

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
                let entry = match g.daemon.get_entry(path.clone()) {
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
                match g.daemon.get_object(entry.id) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
            AccessMode::CopyOnWrite => {
                if let Ok(Some(entry)) = g.daemon.get_entry(path.clone()) {
                    if kind_from_entry(&entry) == Some(FileType::Symlink) {
                        match g.daemon.get_object(entry.id) {
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
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "mkdir");
        let access = self.policy.classify(&path);

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
                if g.daemon.put_dir(path.clone(), meta).is_err() {
                    reply.error(Errno::EIO);
                    return;
                }
                match g.stat_path(&path, &access) {
                    Ok((_k, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
                    Err(code) => reply.error(errno(code)),
                }
            }
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.lock();
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "unlink");
        let access = self.policy.classify(&path);

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::remove_file(&real) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly => match g.daemon.delete_file(path) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
            AccessMode::CopyOnWrite => match g.daemon.get_entry(path.clone()) {
                Ok(Some(entry)) => {
                    if entry.entry_type == EntryType::Whiteout {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    match g.daemon.delete_file(path) {
                        Ok(()) => reply.ok(),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
                Ok(None) => {
                    let real = g.real_path(&path);
                    if real.exists() {
                        match g.daemon.put_whiteout(path) {
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
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "rmdir");
        let access = self.policy.classify(&path);

        match access {
            AccessMode::Passthrough => {
                let real = g.real_path(&path);
                match fs::remove_dir(&real) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let children = match g.daemon.read_dir_all(path.clone()) {
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
                    && matches!(g.daemon.get_entry(path.clone()), Ok(None))
                {
                    let real = g.real_path(&path);
                    if real.exists() {
                        match g.daemon.put_whiteout(path) {
                            Ok(()) => reply.ok(),
                            Err(_) => reply.error(Errno::EIO),
                        }
                    } else {
                        reply.error(Errno::ENOENT)
                    }
                } else {
                    match g.daemon.delete_file(path) {
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
        let from_parent = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let to_parent = match g.path_of(newparent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let from = normalize_abs(&from_parent.join(name));
        let to = normalize_abs(&to_parent.join(newname));

        self.maybe_log(req, &mut g, &from, "rename");
        let from_mode = self.policy.classify(&from);
        let to_mode = self.policy.classify(&to);
        if from_mode != to_mode {
            reply.error(Errno::EPERM);
            return;
        }

        match from_mode {
            AccessMode::Passthrough => {
                let real_from = g.real_path(&from);
                let real_to = g.real_path(&to);
                match fs::rename(&real_from, &real_to) {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(io_errno(&e))),
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                match g.daemon.rename_file(from, to) {
                    Ok(()) => reply.ok(),
                    Err(_) => reply.error(Errno::EIO),
                }
            }
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
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "symlink");
        let access = self.policy.classify(&path);

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
                if g.daemon.put_file(path.clone(), bytes, meta).is_err() {
                    reply.error(Errno::EIO);
                    return;
                }
                match g.stat_path(&path, &access) {
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

        self.maybe_log(req, &mut g, &path, "open");
        let access = self.policy.classify(&path);
        let accmode = flags.0 & libc::O_ACCMODE;
        let wants_write = accmode != libc::O_RDONLY;
        let trunc = (flags.0 & libc::O_TRUNC) != 0;

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
                let entry = match g.daemon.get_entry(path.clone()) {
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
                let daemon_entry = match g.daemon.get_entry(path.clone()) {
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
        let parent_path = match g.path_of(parent).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                self.req_err(req, "create", None, libc::ENOENT, "parent inode missing");
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let path = normalize_abs(&parent_path.join(name));
        self.maybe_log(req, &mut g, &path, "create");
        let access = self.policy.classify(&path);

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
                if g.daemon
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
                let obj = match g.daemon.get_entry(path.clone()) {
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
        let mut g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }

        let mut of = match g.open_files.remove(&fh.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let root = g.root.clone();
        let res = of.read_at(offset, size, &root, &mut g.daemon);
        g.open_files.insert(fh.0, of);

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
        let mut g = self.lock();
        if g.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }

        let mut of = match g.open_files.remove(&fh.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let root = g.root.clone();
        let res = of.write_at(offset, data, &root, &mut g.daemon);
        g.open_files.insert(fh.0, of);

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
        let mut g = self.lock();

        if g.path_of(ino_in).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        if g.path_of(ino_out).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }

        let mut of_in = match g.open_files.remove(&fh_in.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let mut of_out = match g.open_files.remove(&fh_out.0) {
            Some(v) => v,
            None => {
                g.open_files.insert(fh_in.0, of_in);
                reply.error(Errno::EBADF);
                return;
            }
        };

        let root = g.root.clone();
        let read_result = of_in.copy_from(offset_in, len, &root, &mut g.daemon);

        match read_result {
            Ok(data) => {
                let write_result = of_out.write_at(offset_out, &data, &root, &mut g.daemon);
                g.open_files.insert(fh_in.0, of_in);
                g.open_files.insert(fh_out.0, of_out);
                match write_result {
                    Ok(n) => reply.written(n as u32),
                    Err(code) => reply.error(errno(code)),
                }
            }
            Err(code) => {
                g.open_files.insert(fh_in.0, of_in);
                g.open_files.insert(fh_out.0, of_out);
                reply.error(errno(code));
            }
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
        let mut g = self.lock();
        let mut of = match g.open_files.remove(&fh.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let res = of.flush_to_daemon(&mut g.daemon);
        g.open_files.insert(fh.0, of);

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
        let mut g = self.lock();
        let mut of = match g.open_files.remove(&fh.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        if flush {
            if let Err(code) = of.flush_to_daemon(&mut g.daemon) {
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
        let mut g = self.lock();
        let mut of = match g.open_files.remove(&fh.0) {
            Some(v) => v,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let res = of.flush_to_daemon(&mut g.daemon);
        g.open_files.insert(fh.0, of);

        match res {
            Ok(()) => reply.ok(),
            Err(code) => reply.error(errno(code)),
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
        let path = match g.path_of(ino).map(Path::to_path_buf) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let access = self.policy.classify(&path);
        let mut merged: BTreeMap<Vec<u8>, (PathBuf, FileType)> = BTreeMap::new();

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
            if let Ok(entries) = g.daemon.read_dir_all(path.clone()) {
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
        match nix::sys::statvfs::statvfs(&g.root) {
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
}
