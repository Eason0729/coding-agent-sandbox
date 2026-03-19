use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::{fs, io};

use fuser::*;

use crate::error::{Error, Result};
use crate::fuse::inner::Inner;
use crate::fuse::open_file::{FileState, OpenFile, TTL};
use crate::fuse::policy::{AccessMode, Policy};
use crate::syncing::proto::{EntryType, FileMetadata};

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

        match self.policy.classify(path) {
            AccessMode::Passthrough => self.inner.stat_real_path(path),
            AccessMode::FuseOnly => self.inner.stat_fuse_path(path, &mut client),
            AccessMode::CopyOnWrite => {
                let entry = client.get_entry(path.to_path_buf()).map_err(Error::from)?;
                match entry {
                    Some(e) if e.entry_type == EntryType::Whiteout => {
                        Err(Error::from(std::io::Error::from_raw_os_error(libc::ENOENT)))
                    }
                    Some(_) => self.inner.stat_fuse_path(path, &mut client),
                    None => self.inner.stat_real_path(path),
                }
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

    fn open_options_from_flags(flags: i32) -> std::fs::OpenOptions {
        let mut opts = fs::OpenOptions::new();
        let accmode = flags & libc::O_ACCMODE;
        match accmode {
            libc::O_RDONLY => {
                opts.read(true);
            }
            libc::O_WRONLY => {
                opts.write(true);
            }
            libc::O_RDWR => {
                opts.read(true).write(true);
            }
            _ => {
                opts.read(true);
            }
        }
        opts.custom_flags(flags);
        if flags & libc::O_TRUNC != 0 {
            opts.truncate(true);
        }
        opts
    }

    fn has_write_intent(flags: i32) -> bool {
        let accmode = flags & libc::O_ACCMODE;
        accmode == libc::O_WRONLY
            || accmode == libc::O_RDWR
            || (flags & libc::O_TRUNC) != 0
            || (flags & libc::O_APPEND) != 0
            || (flags & libc::O_CREAT) != 0
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

    fn file_from_openfile(of: &mut OpenFile) -> &mut std::fs::File {
        match &mut of.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => file,
        }
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

macro_rules! get_path {
    ($self:ident, $rep: ident, $parent: expr) => {
        match $self.path_of($p) {
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
            InitFlags::FUSE_WRITEBACK_CACHE,
            InitFlags::FUSE_WRITEBACK_CACHE,
            InitFlags::FUSE_BIG_WRITES,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_EXPORT_SUPPORT,
            InitFlags::FUSE_PASSTHROUGH,
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
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err_to_errno(&err)),
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
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };

        if let Some(fh) = fh {
            if let Some(mut of) = self.open_files.get_mut(&fh.0) {
                let file = Self::file_from_openfile(&mut of);
                if let Some(sz) = size {
                    if let Err(e) = file.set_len(sz) {
                        reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                }
                if mode.is_some() || uid.is_some() || gid.is_some() {
                    let mut perms = match file.metadata() {
                        Ok(m) => m.permissions(),
                        Err(e) => {
                            reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                            return;
                        }
                    };
                    if let Some(m) = mode {
                        perms.set_mode(m & 0o7777);
                        let _ = file.set_permissions(perms);
                    }
                    let _ = nix::unistd::fchown(
                        file.as_raw_fd(),
                        uid.map(nix::unistd::Uid::from_raw),
                        gid.map(nix::unistd::Gid::from_raw),
                    );
                }
                if let Ok(meta) = file.metadata() {
                    let fmeta = Self::meta_from_stat(&meta);
                    let _ = daemon.put_file_meta(path.clone(), fmeta);
                }
                match self.stat_path(&path) {
                    Ok((_k, attr)) => reply.attr(&TTL, &attr),
                    Err(err) => reply.error(err_to_errno(&err)),
                }
                return;
            }
        }

        match self.policy.classify(&path) {
            AccessMode::Passthrough => {
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
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                if let Ok(Some(mut m)) = daemon.get_file_meta(path.clone()) {
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
                    let _ = daemon.put_file_meta(path.clone(), m);
                }
            }
        }

        match self.stat_path(&path) {
            Ok((_kind, attr)) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err_to_errno(&err)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = match self.path_of(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        match daemon.get_entry(path.clone()) {
            Ok(Some(entry)) if entry.entry_type == EntryType::Symlink => {
                let data = entry.symlink_target.unwrap_or_default();
                reply.data(&data);
            }
            Ok(Some(entry)) if entry.entry_type == EntryType::Whiteout => {
                reply.error(Errno::ENOENT);
            }
            _ => match fs::read_link(&path) {
                Ok(target) => reply.data(target.as_os_str().as_bytes()),
                Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
            },
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
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        match self.policy.classify(&path) {
            AccessMode::Passthrough => {
                let res = fs::create_dir(&path).and_then(|_| {
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o7777))
                });
                if let Err(e) = res {
                    reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                    return;
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let meta =
                    Self::file_meta_now(0, libc::S_IFDIR | (mode & 0o7777), req.uid(), req.gid());
                if let Err(err) = daemon.put_dir(path.clone(), meta) {
                    reply.error(err_to_errno(&Error::from(err)));
                    return;
                }
                let _ = daemon.delete_whiteout(path.clone());
            }
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
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        match self.policy.classify(&path) {
            AccessMode::Passthrough => match fs::remove_file(&path) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
            },
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let _ = daemon.delete_file(path.clone());
                match daemon.put_whiteout(path) {
                    Ok(()) => reply.ok(),
                    Err(err) => reply.error(err_to_errno(&Error::from(err))),
                }
            }
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, component: &OsStr, reply: ReplyEmpty) {
        let path = get_path!(self, reply, parent, component);

        let access = self.policy.classify(&path);

        match access {
            AccessMode::Passthrough => match fs::remove_dir(&path) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or_default())),
            },
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let mut daemon = match self.connect_daemon() {
                    Ok(v) => v,
                    Err(err) => {
                        reply.error(err_to_errno(&err));
                        return;
                    }
                };
                let _ = daemon.delete_file(path.clone());
                let descendants = Self::recursive_real_descendants(&path);
                if let Err(err) = daemon.put_whiteout(path.clone()) {
                    reply.error(err_to_errno(&Error::from(err)));
                    return;
                }
                for p in descendants {
                    let _ = daemon.put_whiteout(p);
                }
                reply.ok()
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
        let from = get_path!(self, reply, parent, name);
        let to = get_path!(self, reply, newparent, newname);
        match self.policy.classify(&from) {
            AccessMode::Passthrough => match fs::rename(&from, &to) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
            },
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let mut daemon = match self.connect_daemon() {
                    Ok(v) => v,
                    Err(err) => {
                        reply.error(err_to_errno(&err));
                        return;
                    }
                };
                let is_dir = daemon
                    .get_entry(from.clone())
                    .ok()
                    .flatten()
                    .map(|e| e.entry_type == EntryType::Dir)
                    .unwrap_or(false);
                let res = if is_dir {
                    daemon.rename_tree(from.clone(), to.clone())
                } else {
                    daemon.rename_file(from.clone(), to.clone())
                };
                if let Err(err) = res {
                    reply.error(err_to_errno(&Error::from(err)));
                    return;
                }
                let _ = daemon.delete_whiteout(to);
                reply.ok();
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
        let path = get_path!(self, reply, parent, name);
        match self.policy.classify(&path) {
            AccessMode::Passthrough => match std::os::unix::fs::symlink(target, &path) {
                Ok(()) => {}
                Err(e) => {
                    reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                    return;
                }
            },
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let mut daemon = match self.connect_daemon() {
                    Ok(v) => v,
                    Err(err) => {
                        reply.error(err_to_errno(&err));
                        return;
                    }
                };
                let meta = Self::file_meta_now(
                    target.as_os_str().as_bytes().len() as u64,
                    libc::S_IFLNK | 0o777,
                    req.uid(),
                    req.gid(),
                );
                if let Err(err) =
                    daemon.put_symlink(path.clone(), target.as_os_str().as_bytes().to_vec(), meta)
                {
                    reply.error(err_to_errno(&Error::from(err)));
                    return;
                }
                let _ = daemon.delete_whiteout(path.clone());
            }
        }
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
        let raw_flags = flags.0;
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let access = self.policy.classify(&path);

        let state = match access {
            AccessMode::Passthrough => match Self::open_options_from_flags(raw_flags).open(&path) {
                Ok(file) => FileState::PassthroughReal { file },
                Err(e) => {
                    reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                    return;
                }
            },
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let entry = match daemon.get_entry(path.clone()) {
                    Ok(v) => v,
                    Err(err) => {
                        reply.error(err_to_errno(&Error::from(err)));
                        return;
                    }
                };
                if let Some(e) = entry {
                    if e.entry_type == EntryType::Whiteout {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    if let Some(oid) = e.object_id {
                        match daemon.get_object_path(oid) {
                            Ok(object_path) => {
                                match Self::open_options_from_flags(raw_flags).open(&object_path) {
                                    Ok(file) => FileState::PassthroughObject {
                                        file,
                                        object_id: oid,
                                    },
                                    Err(err) => {
                                        reply.error(Errno::from_i32(
                                            err.raw_os_error().unwrap_or(libc::EIO),
                                        ));
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                reply.error(err_to_errno(&Error::from(err)));
                                return;
                            }
                        }
                    } else {
                        match Self::open_options_from_flags(raw_flags).open(&path) {
                            Ok(file) => FileState::PassthroughReal { file },
                            Err(err) => {
                                reply.error(Errno::from_i32(
                                    err.raw_os_error().unwrap_or(libc::EIO),
                                ));
                                return;
                            }
                        }
                    }
                } else if access == AccessMode::CopyOnWrite && !Self::has_write_intent(raw_flags) {
                    match Self::open_options_from_flags(raw_flags).open(&path) {
                        Ok(file) => FileState::PassthroughReal { file },
                        Err(err) => {
                            reply.error(Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)));
                            return;
                        }
                    }
                } else {
                    if let Ok(stat) = fs::symlink_metadata(&path) {
                        let meta = Self::meta_from_stat(&stat);
                        let (oid, object_path) = match daemon.ensure_file_object(path.clone(), meta)
                        {
                            Ok(v) => v,
                            Err(err) => {
                                reply.error(err_to_errno(&Error::from(err)));
                                return;
                            }
                        };
                        let real_len = stat.size();
                        if let Ok(obj_meta) = fs::metadata(&object_path) {
                            if obj_meta.len() == 0 && real_len > 0 {
                                let _ = fs::copy(&path, &object_path);
                            }
                        }
                        let _ = daemon.delete_whiteout(path.clone());
                        match Self::open_options_from_flags(raw_flags).open(&object_path) {
                            Ok(file) => FileState::PassthroughObject {
                                file,
                                object_id: oid,
                            },
                            Err(err) => {
                                reply.error(Errno::from_i32(
                                    err.raw_os_error().unwrap_or(libc::EIO),
                                ));
                                return;
                            }
                        }
                    } else {
                        let now_meta =
                            Self::file_meta_now(0, libc::S_IFREG | 0o644, req.uid(), req.gid());
                        let (oid, object_path) =
                            match daemon.ensure_file_object(path.clone(), now_meta) {
                                Ok(v) => v,
                                Err(err) => {
                                    reply.error(err_to_errno(&Error::from(err)));
                                    return;
                                }
                            };
                        let _ = daemon.delete_whiteout(path.clone());
                        match Self::open_options_from_flags(raw_flags).open(&object_path) {
                            Ok(file) => FileState::PassthroughObject {
                                file,
                                object_id: oid,
                            },
                            Err(e) => {
                                reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                                return;
                            }
                        }
                    }
                }
            }
        };

        let fh = self.alloc_fh();
        let open_result = match &state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => {
                if let Some(existing) = self.inner.backing_ids.get(&path) {
                    Ok(existing.clone())
                } else {
                    reply.open_backing(file).map(Arc::new)
                }
            }
        };
        self.open_files.insert(
            fh,
            OpenFile {
                ino: self.inodes.get_or_insert(&path),
                state,
            },
        );
        match open_result {
            Ok(backing_id) => {
                self.inner
                    .backing_ids
                    .insert(path.clone(), backing_id.clone());
                reply.opened_passthrough(
                    FileHandle(fh),
                    FopenFlags::FOPEN_PASSTHROUGH,
                    &backing_id,
                );
            }
            Err(_) => reply.opened(FileHandle(fh), FopenFlags::empty()),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        if let Some(path) = self.path_of(ino) {
            self.inner.backing_ids.remove(&path);
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
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let raw_flags = flags;

        let state = match self.policy.classify(&path) {
            AccessMode::Passthrough => {
                let mut opts = Self::open_options_from_flags(raw_flags);
                opts.create(true).mode(mode);
                match opts.open(&path) {
                    Ok(file) => FileState::PassthroughReal { file },
                    Err(e) => {
                        reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                }
            }
            AccessMode::FuseOnly | AccessMode::CopyOnWrite => {
                let meta =
                    Self::file_meta_now(0, libc::S_IFREG | (mode & 0o7777), req.uid(), req.gid());
                let (oid, object_path) = match daemon.ensure_file_object(path.clone(), meta) {
                    Ok(v) => v,
                    Err(err) => {
                        reply.error(err_to_errno(&Error::from(err)));
                        return;
                    }
                };
                let _ = daemon.delete_whiteout(path.clone());
                let mut opts = Self::open_options_from_flags(raw_flags);
                opts.create(true).mode(mode);
                match opts.open(&object_path) {
                    Ok(file) => FileState::PassthroughObject {
                        file,
                        object_id: oid,
                    },
                    Err(e) => {
                        reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                }
            }
        };

        let fh = self.alloc_fh();
        let open_result = match &state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => {
                if let Some(existing) = self.inner.backing_ids.get(&path) {
                    Ok(existing.clone())
                } else {
                    reply.open_backing(file).map(Arc::new)
                }
            }
        };
        self.open_files.insert(
            fh,
            OpenFile {
                ino: self.inodes.get_or_insert(&path),
                state,
            },
        );
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
        match open_result {
            Ok(backing_id) => {
                self.inner
                    .backing_ids
                    .insert(path.clone(), backing_id.clone());
                reply.created_passthrough(
                    &TTL,
                    &attr,
                    fuser::Generation(0),
                    FileHandle(fh),
                    FopenFlags::empty(),
                    &backing_id,
                );
            }
            Err(_) => reply.created(
                &TTL,
                &attr,
                fuser::Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
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
        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        let root = PathBuf::from_str("/").unwrap();
        let data = match self.open_files.get_mut(&fh_in.0) {
            Some(mut of) => match of.copy_from(offset_in, len, &root, &mut daemon) {
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
            Some(mut of) => match of.write_at(offset_out, &data, &root, &mut daemon) {
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
            Some(mut of) => {
                let file = Self::file_from_openfile(&mut of);
                match io::Seek::seek(file, wh) {
                    Ok(v) => v,
                    Err(e) => {
                        reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        return;
                    }
                }
            }
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

        if self.policy.classify(&path) != AccessMode::FuseOnly {
            if let Ok(rd) = fs::read_dir(&path) {
                for ent in rd.flatten() {
                    let name = ent.file_name().as_bytes().to_vec();
                    let p = ent.path();
                    let kind = match ent.file_type() {
                        Ok(t) if t.is_dir() => FileType::Directory,
                        Ok(t) if t.is_symlink() => FileType::Symlink,
                        _ => FileType::RegularFile,
                    };
                    items.insert(name, (kind, p));
                }
            }
        }

        let mut daemon = match self.connect_daemon() {
            Ok(v) => v,
            Err(err) => {
                reply.error(err_to_errno(&err));
                return;
            }
        };
        if let Ok(fuse_entries) = daemon.read_dir_all(path.clone()) {
            let mut whiteouts = HashSet::new();
            for (child_path, entry) in fuse_entries {
                let Some(name) = child_path.file_name() else {
                    continue;
                };
                let key = name.as_bytes().to_vec();
                if entry.entry_type == EntryType::Whiteout {
                    whiteouts.insert(key.clone());
                    items.remove(&key);
                    continue;
                }
                let kind = match entry.entry_type {
                    EntryType::Dir => FileType::Directory,
                    EntryType::Symlink => FileType::Symlink,
                    EntryType::File => FileType::RegularFile,
                    EntryType::Whiteout => continue,
                };
                items.insert(key, (kind, child_path));
            }
            for w in whiteouts {
                items.remove(&w);
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
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
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
}
