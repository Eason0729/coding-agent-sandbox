use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::NamedTempFile;

use crate::syncing::client::SyncClient;
use crate::syncing::proto::{EntryType, FileMetadata, FuseEntry};

pub const TTL: Duration = Duration::from_secs(1);

pub enum FileState {
    Passthrough {
        file: File,
    },
    CowClean {
        object_id: Option<u64>,
    },
    CowDirty {
        tmp: NamedTempFile,
        object_id_before: Option<u64>,
    },
    FuseOnlyNew {
        tmp: NamedTempFile,
    },
    FuseOnlyClean {
        object_id: u64,
    },
    FuseOnlyDirty {
        tmp: NamedTempFile,
        object_id: u64,
    },
}

pub struct OpenFile {
    pub path: PathBuf,
    pub state: FileState,
}

pub fn tmp_as_file(tmp: &NamedTempFile) -> std::mem::ManuallyDrop<File> {
    unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(tmp.as_raw_fd())) }
}

impl OpenFile {
    pub fn materialize(&mut self, root: &Path, daemon: &mut SyncClient) -> Result<(), libc::c_int> {
        match &mut self.state {
            FileState::CowDirty { .. } => return Ok(()),
            FileState::CowClean { object_id } => {
                let object_id = *object_id;
                let bytes = if let Some(id) = object_id {
                    daemon.get_object(id).map_err(|_| libc::EIO)?
                } else {
                    let real_path = root.join(self.path.strip_prefix("/").unwrap_or(&self.path));
                    std::fs::read(&real_path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?
                };
                let mut tmp = NamedTempFile::new().map_err(|_| libc::EIO)?;
                tmp.write_all(&bytes).map_err(|_| libc::EIO)?;
                tmp.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
                self.state = FileState::CowDirty {
                    tmp,
                    object_id_before: object_id,
                };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn flush_to_daemon(&mut self, daemon: &mut SyncClient) -> Result<(), libc::c_int> {
        match &mut self.state {
            FileState::CowDirty { tmp, .. } | FileState::FuseOnlyDirty { tmp, .. } => {
                tmp.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
                let mut buf = Vec::new();
                {
                    let fd = tmp.as_raw_fd();
                    let mut f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
                    f.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
                    f.read_to_end(&mut buf).map_err(|_| libc::EIO)?;
                }
                let real_meta = nix::sys::stat::fstat(tmp.as_raw_fd()).ok();
                let uid = real_meta.as_ref().map(|m| m.st_uid).unwrap_or(0);
                let gid = real_meta.as_ref().map(|m| m.st_gid).unwrap_or(0);
                let mode = real_meta.as_ref().map(|m| m.st_mode).unwrap_or(0o644);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let file_meta = FileMetadata {
                    size: buf.len() as u64,
                    mode,
                    uid,
                    gid,
                    mtime: now,
                    atime: now,
                    ctime: now,
                };
                daemon
                    .put_file(self.path.clone(), buf, file_meta)
                    .map_err(|_| libc::EIO)?;
                Ok(())
            }
            FileState::FuseOnlyNew { tmp } => {
                tmp.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
                let mut buf = Vec::new();
                {
                    let fd = tmp.as_raw_fd();
                    let mut f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
                    f.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
                    f.read_to_end(&mut buf).map_err(|_| libc::EIO)?;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let file_meta = FileMetadata {
                    size: buf.len() as u64,
                    mode: 0o644,
                    uid: 0,
                    gid: 0,
                    mtime: now,
                    atime: now,
                    ctime: now,
                };
                daemon
                    .put_file(self.path.clone(), buf, file_meta)
                    .map_err(|_| libc::EIO)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn read_at(
        &mut self,
        offset: u64,
        size: u32,
        root: &Path,
        daemon: &mut SyncClient,
    ) -> Result<Vec<u8>, libc::c_int> {
        match &mut self.state {
            FileState::Passthrough { file } => {
                if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                let mut buf = vec![0u8; size as usize];
                match file.read(&mut buf) {
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                if let Err(e) = tmp.seek(SeekFrom::Start(offset)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                let mut buf = vec![0u8; size as usize];
                let mut f = tmp_as_file(tmp);
                f.seek(SeekFrom::Start(offset)).ok();
                match f.read(&mut buf) {
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            FileState::CowClean { object_id } => {
                let object_id = *object_id;
                if let Some(id) = object_id {
                    match daemon.get_object(id) {
                        Ok(bytes) => {
                            let start = offset as usize;
                            let end = (offset as usize + size as usize).min(bytes.len());
                            if start >= bytes.len() {
                                Ok(vec![])
                            } else {
                                Ok(bytes[start..end].to_vec())
                            }
                        }
                        Err(_) => Err(libc::EIO),
                    }
                } else {
                    let real_path = root.join(self.path.strip_prefix("/").unwrap_or(&self.path));
                    match File::open(&real_path) {
                        Ok(mut f) => {
                            if let Err(e) = f.seek(SeekFrom::Start(offset)) {
                                return Err(e.raw_os_error().unwrap_or(libc::EIO));
                            }
                            let mut buf = vec![0u8; size as usize];
                            match f.read(&mut buf) {
                                Ok(n) => Ok(buf[..n].to_vec()),
                                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                            }
                        }
                        Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                    }
                }
            }
            FileState::FuseOnlyClean { object_id } => {
                let id = *object_id;
                match daemon.get_object(id) {
                    Ok(bytes) => {
                        let start = offset as usize;
                        let end = (offset as usize + size as usize).min(bytes.len());
                        if start >= bytes.len() {
                            Ok(vec![])
                        } else {
                            Ok(bytes[start..end].to_vec())
                        }
                    }
                    Err(_) => Err(libc::EIO),
                }
            }
        }
    }

    pub fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
        root: &Path,
        daemon: &mut SyncClient,
    ) -> Result<usize, libc::c_int> {
        match &mut self.state {
            FileState::Passthrough { file } => {
                if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                match file.write(data) {
                    Ok(n) => Ok(n),
                    Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                let mut f = tmp_as_file(tmp);
                if let Err(e) = f.seek(SeekFrom::Start(offset)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                match f.write(data) {
                    Ok(n) => Ok(n),
                    Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            FileState::CowClean { .. } => {
                self.materialize(root, daemon)?;
                if let FileState::CowDirty { tmp, .. } = &mut self.state {
                    let mut f = tmp_as_file(tmp);
                    if let Err(e) = f.seek(SeekFrom::Start(offset)) {
                        return Err(e.raw_os_error().unwrap_or(libc::EIO));
                    }
                    match f.write(data) {
                        Ok(n) => Ok(n),
                        Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                    }
                } else {
                    Err(libc::EIO)
                }
            }
            FileState::FuseOnlyClean { object_id } => {
                let id = *object_id;
                match daemon.get_object(id) {
                    Ok(bytes) => {
                        let mut tmp = NamedTempFile::new().map_err(|_| libc::EIO)?;
                        let _ = tmp.write_all(&bytes);
                        let _ = tmp.seek(SeekFrom::Start(offset));
                        {
                            let mut f = tmp_as_file(&tmp);
                            f.seek(SeekFrom::Start(offset)).ok();
                            match f.write(data) {
                                Ok(n) => {
                                    self.state = FileState::FuseOnlyDirty { tmp, object_id: id };
                                    Ok(n)
                                }
                                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                            }
                        }
                    }
                    Err(_) => Err(libc::EIO),
                }
            }
        }
    }
}
