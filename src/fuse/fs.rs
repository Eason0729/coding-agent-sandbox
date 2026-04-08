use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::{fs, io};

use fuser::*;

use crate::error::{Error, Result};
use crate::fuse::decision::{
    choose_visible_child, decide_create, decide_mkdir, decide_open_with_transitions,
    decide_readdir, decide_readlink, decide_rename, decide_rmdir, decide_setattr, decide_stat,
    decide_unlink, validate_readdir_decision, OpenDecision, SetattrDecision, StatDecision,
};
use crate::fuse::executor::{
    execute_create, execute_mkdir, execute_open, execute_readlink, execute_rename, execute_rmdir,
    execute_symlink, execute_unlink,
};
use crate::fuse::inner::Inner;
use crate::fuse::open_file::{OpenFile, TTL};
use crate::fuse::policy::Policy;
use crate::fuse::state_loader::{
    load_create_state, load_mkdir_state, load_open_state, load_readdir_state, load_readlink_state,
    load_rename_state, load_rmdir_state, load_setattr_state, load_stat_state, load_unlink_state,
};
use crate::syncing::proto::FileMetadata;

fn err_to_errno(err: &Error) -> Errno {
    match err {
        Error::Io(ioe) => ioe
            .raw_os_error()
            .map(Errno::from_i32)
            .unwrap_or(Errno::EIO),
        Error::SyncingClient(client_err) => match client_err {
            crate::syncing::ClientError::NotFound => Errno::ENOENT,
            crate::syncing::ClientError::Io(ioe) => ioe
                .raw_os_error()
                .map(Errno::from_i32)
                .unwrap_or(Errno::EIO),
            crate::syncing::ClientError::Serialize(_) => Errno::EIO,
            crate::syncing::ClientError::Server(_) => Errno::EIO,
        },
    }
}

macro_rules! reply_error {
    ($reply:expr, $err: expr) => {
        match ($err) {
            Ok(v) => v,
            Err(err) => {
                $reply.error(err);
                return;
            }
        }
    };
}

#[derive(Clone)]
pub struct CasFuseFs {
    inner: Arc<Inner>,
    policy: Arc<dyn Policy>,
}

impl Deref for CasFuseFs {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl CasFuseFs {
    pub fn new(daemon_sock: PathBuf, policy: Arc<dyn Policy>) -> Self {
        Self {
            inner: Arc::new(Inner::new(daemon_sock)),
            policy,
        }
    }

    fn req_start(&self, req: &Request, op: &str, path: Option<&Path>, detail: &str) {
        let p = path
            .map(|v| v.display().to_string())
            .unwrap_or_else(|| "-".to_string());

        if let Some(path) = path {
            if self.policy.should_log(path) {
                if let Ok(mut daemon) = self.inner.connect_daemon() {
                    daemon
                        .log_access(path.to_path_buf(), op.to_string(), req.pid())
                        .ok();
                }
            }
        }

        log::debug!(
            "fuse.{op}.start pid={} uid={} gid={} path={} {}",
            req.pid(),
            req.uid(),
            req.gid(),
            p,
            detail
        );
    }

    fn stat_path(&self, path: &Path) -> Result<(FileType, fuser::FileAttr)> {
        let mut client = self.get_sync_client()?;

        let state = load_stat_state(self.policy.as_ref(), path, &mut client)?;
        match decide_stat(&state) {
            StatDecision::UseReal => self.inner.stat_real_path(path),
            StatDecision::UseFuse => self.inner.stat_fuse_path(path, &mut client),
            StatDecision::NotFound => {
                Err(Error::from(std::io::Error::from_raw_os_error(libc::ENOENT)))
            }
        }
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn meta_from_stat(stat: &std::fs::Metadata) -> FileMetadata {
        FileMetadata {
            size: stat.size(),
            mode: stat.mode(),
            uid: stat.uid(),
            gid: stat.gid(),
            mtime: stat.mtime().max(0) as u64,
            atime: stat.atime().max(0) as u64,
            ctime: stat.ctime().max(0) as u64,
        }
    }

    fn file_meta_now(size: u64, mode: u32, uid: u32, gid: u32) -> FileMetadata {
        let now = Self::now_unix();
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

    fn recursive_real_descendants(path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let iter = match fs::read_dir(&dir) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for item in iter.flatten() {
                let p = item.path();
                out.push(p.clone());
                if item.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                }
            }
        }
        out
    }
}

/// Check if component is a `export`(man page term)
///
/// `export` is OS-specific, but for unix '..' is fine.
fn check_export_component(component: &OsStr) -> bool {
    let component = component.as_encoded_bytes();
    if component.len() == 2 && component[0] == b'.' && component[1] == b'.' {
        return true;
    }

    false
}

/// Returns true if `path` ends with `-wal` (SQLite WAL journal file).
///
/// The `-wal` file needs `FOPEN_DIRECT_IO` to bypass the kernel page cache
/// and ensure write-through semantics for journal durability.
///
/// The `-shm` file is intentionally excluded: it is mmap-ed with `MAP_SHARED`
/// for the WAL-index, and the kernel's direct-I/O mmap path for FUSE files
/// has known edge cases. Cached mode handles `MAP_SHARED` correctly.
fn is_sqlite_wal_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with("-wal"))
        .unwrap_or(false)
}

macro_rules! get_path {
    ($self:ident, $rep: ident, $parent: expr) => {
        match $self.path_of($parent) {
            Some(mut path) => path,
            None => {
                $rep.error(Errno::ENOENT);
                return;
            }
        };
    };
    ($self:ident, $reply: ident, $parent: expr, $comp: ident) => {
        match $self.path_of($parent) {
            Some(mut path) => {
                if check_export_component($comp) {
                    path.pop();
                } else {
                    path.push($comp)
                };
                path
            }
            None => {
                $reply.error(Errno::ENOENT);
                return;
            }
        }
    };
}

impl Filesystem for CasFuseFs {
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        const FLAGS: &[InitFlags] = &[
            InitFlags::FUSE_ASYNC_READ,
            InitFlags::FUSE_BIG_WRITES,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_EXPORT_SUPPORT,
            InitFlags::FUSE_PASSTHROUGH,
            InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP,
            InitFlags::FUSE_POSIX_LOCKS,
        ];
        for flag in FLAGS {
            config.add_capabilities(*flag).ok();
        }
        let caps = config.capabilities();
        log::debug!(
            "fuse.init pid={} uid={} gid={} caps={:?}",
            req.pid(),
            req.uid(),
            req.gid(),
            caps,
        );
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, component: &OsStr, reply: ReplyEntry) {
        self.req_start(
            req,
            "lookup",
            None,
            &format!("parent={} name={}", parent.0, component.to_string_lossy()),
        );
        let path = get_path!(self, reply, parent, component);

        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(err) => reply.error(err_to_errno(&err)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.req_start(req, "getattr", None, &format!("ino={}", ino.0));
        let path = reply_error!(reply, self.path_of(ino).ok_or(Errno::ENOENT));

        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err_to_errno(&err)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        reply.size(0);
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
        let path = reply_error!(reply, self.path_of(ino).ok_or(Errno::ENOENT));

        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let state = load_setattr_state(
            self.policy.as_ref(),
            &path,
            fh.is_some(),
            fh.map(|fh| self.open_files.get(&fh.0).is_some())
                .unwrap_or(false),
            mode,
            uid,
            gid,
            size,
        );
        match decide_setattr(&state) {
            SetattrDecision::UpdateOpenHandle => {
                if let Some(fh) = fh {
                    if let Some(mut of) = self.open_files.get_mut(&fh.0) {
                        if let Some(sz) = size {
                            if let Err(e) = of.as_mut().set_len(sz) {
                                reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                                return;
                            }
                        }
                        if mode.is_some() || uid.is_some() || gid.is_some() {
                            let mut perms = match of.as_mut().metadata() {
                                Ok(m) => m.permissions(),
                                Err(e) => {
                                    reply.error(Errno::from_i32(
                                        e.raw_os_error().unwrap_or(libc::EIO),
                                    ));
                                    return;
                                }
                            };
                            if let Some(m) = mode {
                                perms.set_mode(m & 0o7777);
                                let _ = of.as_mut().set_permissions(perms);
                            }
                            let _ = nix::unistd::fchown(
                                of.as_mut().as_raw_fd(),
                                uid.map(nix::unistd::Uid::from_raw),
                                gid.map(nix::unistd::Gid::from_raw),
                            );
                        }
                    }
                }
            }
            SetattrDecision::UpdateRealFs => {
                if let Some(sz) = size {
                    if let Err(e) = fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .and_then(|f| f.set_len(sz))
                    {
                        reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                }
                if mode.is_some() || uid.is_some() || gid.is_some() {
                    if let Some(m) = mode {
                        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(m & 0o7777));
                    }
                    let _ = nix::unistd::chown(
                        &path,
                        uid.map(nix::unistd::Uid::from_raw),
                        gid.map(nix::unistd::Gid::from_raw),
                    );
                }
            }
            SetattrDecision::UpdateDaemonMeta => {
                if let Ok(Some(mut m)) = client.get_file_meta(path.clone()) {
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
                    m.ctime = Self::now_unix();
                    let _ = client.put_file_meta(path.clone(), m);
                }
            }
        }

        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err_to_errno(&err)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = reply_error!(reply, self.path_of(ino).ok_or(Errno::ENOENT));

        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let state = reply_error!(
            reply,
            load_readlink_state(self.policy.as_ref(), &path, &mut client)
                .map_err(|err| err_to_errno(&err))
        );
        let decision = decide_readlink(&state);
        match execute_readlink(decision, &mut client, &path) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err_to_errno(&err)),
        }
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
        let path = get_path!(self, reply, parent, name);
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let state = load_mkdir_state(self.policy.as_ref(), &path);
        let decision = decide_mkdir(&state);
        if let Err(err) = execute_mkdir(decision, &mut client, &path, mode, req.uid(), req.gid()) {
            reply.error(err_to_errno(&err));
            return;
        }

        let ino = self.inodes.get_or_insert(&path);
        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(_) => {
                let now = SystemTime::now();
                let attr = FileAttr {
                    ino: INodeNo(ino),
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::Directory,
                    perm: (mode & 0o7777) as u16,
                    nlink: 1,
                    uid: req.uid(),
                    gid: req.gid(),
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                };
                reply.entry(&TTL, &attr, fuser::Generation(0));
            }
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = get_path!(self, reply, parent, name);
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let state = reply_error!(
            reply,
            load_unlink_state(self.policy.as_ref(), &path).map_err(|err| err_to_errno(&err))
        );
        let decision = decide_unlink(&state);
        match execute_unlink(decision, &mut client, &path) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err_to_errno(&err)),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, component: &OsStr, reply: ReplyEmpty) {
        let path = get_path!(self, reply, parent, component);

        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let state = reply_error!(
            reply,
            load_rmdir_state(self.policy.as_ref(), &path).map_err(|err| err_to_errno(&err))
        );
        let decision = decide_rmdir(&state);
        let descendants = Self::recursive_real_descendants(&path);
        match execute_rmdir(decision, &mut daemon, &path, &descendants) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err_to_errno(&err)),
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
        let from = get_path!(self, reply, parent, name);
        let to = get_path!(self, reply, newparent, newname);
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let state = reply_error!(
            reply,
            load_rename_state(self.policy.as_ref(), &from, &to, &mut client)
                .map_err(|err| err_to_errno(&err))
        );
        let decision = decide_rename(&state);
        match execute_rename(decision, &mut client, &from, &to) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err_to_errno(&err)),
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
        let path = get_path!(self, reply, parent, name);
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let meta = Self::file_meta_now(
            target.as_os_str().as_bytes().len() as u64,
            libc::S_IFLNK | 0o777,
            req.uid(),
            req.gid(),
        );
        let access = self.policy.classify(&path);
        reply_error!(
            reply,
            execute_symlink(access, &mut client, &path, target, meta)
                .map_err(|err| err_to_errno(&err))
        );
        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            Err(err) => reply.error(err_to_errno(&err)),
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
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        self.req_start(req, "open", Some(&path), "");
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );

        let need_write = matches!(
            flags.acc_mode(),
            OpenAccMode::O_RDWR | OpenAccMode::O_WRONLY
        );
        let truncate_requested = (flags.0 & libc::O_TRUNC) != 0;

        let state = reply_error!(
            reply,
            load_open_state(
                self.policy.as_ref(),
                &path,
                need_write,
                truncate_requested,
                &mut client,
            )
            .map_err(|err| err_to_errno(&err))
        );
        let (decision, transitions) = decide_open_with_transitions(&state);
        log::debug!(
            "fuse.open.plan path={} need_write={} truncate={} decision={:?} transitions={:?}",
            path.display(),
            need_write,
            truncate_requested,
            decision,
            transitions,
        );
        if matches!(&decision, OpenDecision::NotFound) {
            reply.error(Errno::ENOENT);
            return;
        }
        if matches!(&decision, OpenDecision::Error) {
            reply.error(Errno::EIO);
            return;
        }

        let default_meta = Self::file_meta_now(0, libc::S_IFREG | 0o644, req.uid(), req.gid());
        let opened = reply_error!(
            reply,
            execute_open(decision, &mut client, &path, flags, default_meta)
                .map_err(|err| err_to_errno(&err))
        );

        let state = match opened.object_id {
            Some(id) => OpenFile::PassthroughObject {
                file: opened.file,
                object_id: id,
            },
            None => OpenFile::PassthroughReal { file: opened.file },
        };

        let fh = self.alloc_fh();

        // SQLite WAL/SHM files must NOT use passthrough.
        // FOPEN_PASSTHROUGH + FOPEN_DIRECT_IO conflict in the kernel's async I/O
        // path (used by bun/Drizzle). For these files, use normal FUSE I/O with
        // FOPEN_DIRECT_IO and our setlk/getlk handlers.
        let is_sqlite = is_sqlite_wal_file(&path);

        let backing_id: Option<Arc<BackingId>> = if !is_sqlite {
            match &state {
                OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => {
                    reply.open_backing(file).map(Arc::new).ok()
                }
            }
        } else {
            None
        };

        self.open_files.insert(fh, state);

        let open_flags = if is_sqlite {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };

        match backing_id {
            Some(id) => reply.opened_passthrough(
                FileHandle(fh),
                FopenFlags::FOPEN_PASSTHROUGH | open_flags,
                &id,
            ),
            None => reply.opened(FileHandle(fh), open_flags),
        }
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
        let path = get_path!(self, reply, parent, name);
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let raw_flags = flags;
        let create_state = load_create_state(self.policy.as_ref(), &path);
        let create_decision = decide_create(&create_state);
        let meta = Self::file_meta_now(0, libc::S_IFREG | (mode & 0o7777), req.uid(), req.gid());
        let state = reply_error!(
            reply,
            execute_create(
                create_decision,
                &mut client,
                &path,
                mode,
                OpenFlags(raw_flags),
                meta,
            )
            .map_err(|err| err_to_errno(&err))
        );

        let fh = self.alloc_fh();

        let is_sqlite = is_sqlite_wal_file(&path);

        let backing_id: Option<Arc<BackingId>> = if !is_sqlite {
            match &state {
                OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => {
                    reply.open_backing(file).map(Arc::new).ok()
                }
            }
        } else {
            None
        };

        self.open_files.insert(fh, state);
        let attr = match self.stat_path(&path) {
            Ok((_kind, attr)) => attr,
            Err(_) => {
                let now = SystemTime::now();
                FileAttr {
                    ino: INodeNo(self.inodes.get_or_insert(&path)),
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::RegularFile,
                    perm: (mode & 0o7777) as u16,
                    nlink: 1,
                    uid: req.uid(),
                    gid: req.gid(),
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
        };
        let open_flags = if is_sqlite {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };

        match backing_id {
            Some(id) => {
                reply.created_passthrough(
                    &TTL,
                    &attr,
                    fuser::Generation(0),
                    FileHandle(fh),
                    open_flags,
                    &id,
                );
            }
            None => reply.created(
                &TTL,
                &attr,
                fuser::Generation(0),
                FileHandle(fh),
                open_flags,
            ),
        }
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
        let mut daemon = match self.inner.connect_daemon() {
            Ok(d) => d,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };

        if self.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = PathBuf::from_str("/").unwrap();
        let res = match self.open_files.get_mut(&fh.0) {
            Some(mut of) => of.read_at(offset, size, &root, &mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        match res {
            Ok(buf) => reply.data(&buf),
            Err(err) => reply.error(err_to_errno(&err)),
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
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };

        if self.path_of(ino).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let root = PathBuf::from_str("/").unwrap();
        let res = match self.open_files.get_mut(&fh.0) {
            Some(mut of) => of.write_at(offset, data, &root, &mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        match res {
            Ok(n) => reply.written(n as u32),
            Err(err) => reply.error(err_to_errno(&err)),
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
        if self.path_of(ino_in).is_none() || self.path_of(ino_out).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );
        let root = PathBuf::from_str("/").unwrap();
        let data = match self.open_files.get_mut(&fh_in.0) {
            Some(mut of) => match of.copy_from(offset_in, len, &root, &mut client) {
                Ok(v) => v,
                Err(err) => {
                    reply.error(err_to_errno(&err));
                    return;
                }
            },
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        let written = match self.open_files.get_mut(&fh_out.0) {
            Some(mut of) => match of.write_at(offset_out, &data, &root, &mut client) {
                Ok(v) => v,
                Err(err) => {
                    reply.error(err_to_errno(&err));
                    return;
                }
            },
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        reply.written(written as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let mut daemon = match self.inner.connect_daemon() {
            Ok(d) => d,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let res = match self.inner.open_files.get_mut(&fh.0) {
            Some(mut of) => of.flush_to_daemon(&mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        match res {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err_to_errno(&err)),
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
        if flush {
            let mut daemon = match self.inner.connect_daemon() {
                Ok(d) => d,
                Err(err) => {
                    reply.error(err_to_errno(&err));
                    return;
                }
            };
            if let Some(mut of) = self.inner.open_files.get_mut(&fh.0) {
                let _ = of.flush_to_daemon(&mut daemon);
            }
        }
        self.inner.open_files.remove(&fh.0);
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
        let mut daemon = match self.inner.connect_daemon() {
            Ok(d) => d,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let res = match self.inner.open_files.get_mut(&fh.0) {
            Some(mut of) => of.flush_to_daemon(&mut daemon),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        match res {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err_to_errno(&err)),
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
        let wh = match whence {
            libc::SEEK_SET => io::SeekFrom::Start(offset.max(0) as u64),
            libc::SEEK_CUR => io::SeekFrom::Current(offset),
            libc::SEEK_END => io::SeekFrom::End(offset),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let pos = match self.open_files.get_mut(&fh.0) {
            Some(mut of) => match io::Seek::seek(of.as_mut(), wh) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                    return;
                }
            },
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        reply.offset((pos.min(i64::MAX as u64)) as i64);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.path_of(ino).is_none() {
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
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut items: BTreeMap<Vec<u8>, (FileType, PathBuf)> = BTreeMap::new();

        let mut client = reply_error!(
            reply,
            self.connect_daemon().map_err(|err| err_to_errno(&err))
        );

        let state = reply_error!(
            reply,
            load_readdir_state(self.policy.as_ref(), &path, &mut client)
                .map_err(|err| err_to_errno(&err))
        );
        let decision = decide_readdir(&state);
        reply_error!(
            reply,
            validate_readdir_decision(&state, &decision).map_err(|err| err_to_errno(&err))
        );

        for (name, child_decision) in &decision.per_child {
            if let Some((kind, full)) = choose_visible_child(&state, name, *child_decision) {
                items.insert(name.clone(), (kind, full));
            }
        }

        let mut entries: Vec<(u64, FileType, Vec<u8>)> = Vec::new();
        entries.push((ino.0, FileType::Directory, b".".to_vec()));
        let parent_path = path.parent().unwrap_or(Path::new("/"));
        let pino = self.inodes.get_or_insert(parent_path);
        entries.push((pino, FileType::Directory, b"..".to_vec()));
        for (name, (kind, full)) in items {
            let child_ino = self.inodes.get_or_insert(&full);
            entries.push((child_ino, kind, name));
        }

        let start = offset as usize;
        for (i, (ino_no, kind, name)) in entries.into_iter().enumerate().skip(start) {
            let ok = reply.add(
                INodeNo(ino_no),
                (i + 1) as u64,
                kind,
                OsStr::from_bytes(&name),
            );
            if !ok {
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
        match nix::sys::statvfs::statvfs("/") {
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
        match self.path_of(ino) {
            Some(_) => reply.ok(),
            None => reply.error(Errno::ENOENT),
        }
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
        reply.error(Errno::ENOTSUP);
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let res = match self.open_files.get(&fh.0) {
            Some(of) => {
                let fd = of.as_ref().as_raw_fd();
                unsafe { libc::fallocate(fd, mode, offset as _, length as _) }
            }
            None => -1,
        };
        if res == 0 {
            reply.ok();
        } else {
            reply.error(Errno::from_i32(nix::errno::Errno::last_raw()));
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
        let mut e = PollEvents::empty();
        e.set(PollEvents::POLLIN, true);
        e.set(PollEvents::POLLOUT, true);
        reply.poll(e);
    }

    fn setlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        _pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let res = match self.open_files.get(&fh.0) {
            Some(of) => {
                let fd = of.as_ref().as_raw_fd();
                let flock = libc::flock {
                    l_type: typ as _,
                    l_whence: libc::SEEK_SET as _,
                    l_start: start as _,
                    l_len: if end == u64::MAX {
                        0
                    } else {
                        (end.saturating_sub(start) + 1) as _
                    },
                    l_pid: 0,
                };
                let cmd = if sleep { libc::F_SETLKW } else { libc::F_SETLK };
                unsafe { libc::fcntl(fd, cmd, &flock) }
            }
            None => -1,
        };
        if res == 0 {
            reply.ok();
        } else {
            reply.error(Errno::from_i32(nix::errno::Errno::last_raw()));
        }
    }

    fn getlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let res = match self.open_files.get(&fh.0) {
            Some(of) => {
                let fd = of.as_ref().as_raw_fd();
                let mut flock = libc::flock {
                    l_type: typ as _,
                    l_whence: libc::SEEK_SET as _,
                    l_start: start as _,
                    l_len: if end == u64::MAX {
                        0
                    } else {
                        (end.saturating_sub(start) + 1) as _
                    },
                    l_pid: pid as _,
                };
                let ret = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut flock) };
                if ret == 0 {
                    Some((
                        flock.l_type,
                        flock.l_start as u64,
                        flock.l_len as u64,
                        flock.l_pid as u32,
                    ))
                } else {
                    None
                }
            }
            None => None,
        };
        match res {
            Some((l_type, l_start, l_len, l_pid)) => {
                reply.locked(l_start, l_len, l_type as _, l_pid);
            }
            None => {
                reply.error(Errno::from_i32(nix::errno::Errno::last_raw()));
            }
        }
    }
}
