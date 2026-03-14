use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::NamedTempFile;

use crate::syncing::client::SyncClient;
use crate::syncing::proto::{BytePatch, FileMetadata};

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
    FuseOnlyDirtyRanged {
        object_id: u64,
        patches: Vec<BytePatch>,
        truncate_to: Option<u64>,
        logical_size: u64,
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
            FileState::FuseOnlyDirtyRanged {
                object_id: _,
                patches,
                truncate_to,
                logical_size,
                ..
            } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let file_meta = FileMetadata {
                    size: truncate_to.unwrap_or(*logical_size),
                    mode: libc::S_IFREG | 0o644,
                    uid: 0,
                    gid: 0,
                    mtime: now,
                    atime: now,
                    ctime: now,
                };
                let new_id = daemon
                    .patch_file(
                        self.path.clone(),
                        std::mem::take(patches),
                        *truncate_to,
                        file_meta,
                    )
                    .map_err(|_| libc::EIO)?;
                self.state = FileState::FuseOnlyClean { object_id: new_id };
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
            FileState::FuseOnlyDirtyRanged {
                object_id,
                patches,
                truncate_to,
                logical_size,
            } => {
                let effective_size = truncate_to.unwrap_or(*logical_size);
                if offset >= effective_size {
                    return Ok(vec![]);
                }
                let wanted = (effective_size - offset).min(size as u64) as usize;
                let mut out = vec![0u8; wanted];

                let base_read = daemon
                    .get_object_range(*object_id, offset, wanted as u32)
                    .map_err(|_| libc::EIO)?;
                let base_len = base_read.len().min(out.len());
                out[..base_len].copy_from_slice(&base_read[..base_len]);

                for patch in patches.iter() {
                    let p_start = patch.offset;
                    let p_end = patch.offset.saturating_add(patch.data.len() as u64);
                    let r_start = offset;
                    let r_end = offset.saturating_add(out.len() as u64);
                    let ov_start = p_start.max(r_start);
                    let ov_end = p_end.min(r_end);
                    if ov_start >= ov_end {
                        continue;
                    }
                    let src_start = (ov_start - p_start) as usize;
                    let dst_start = (ov_start - r_start) as usize;
                    let n = (ov_end - ov_start) as usize;
                    out[dst_start..dst_start + n]
                        .copy_from_slice(&patch.data[src_start..src_start + n]);
                }

                Ok(out)
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
                let base_size = daemon
                    .get_entry(self.path.clone())
                    .map_err(|_| libc::EIO)?
                    .map(|e| e.metadata.size)
                    .unwrap_or(0);
                let write_end = offset.saturating_add(data.len() as u64);
                self.state = FileState::FuseOnlyDirtyRanged {
                    object_id: id,
                    patches: vec![BytePatch {
                        offset,
                        data: data.to_vec(),
                    }],
                    truncate_to: None,
                    logical_size: base_size.max(write_end),
                };
                Ok(data.len())
            }
            FileState::FuseOnlyDirtyRanged {
                patches,
                logical_size,
                truncate_to,
                ..
            } => {
                patches.push(BytePatch {
                    offset,
                    data: data.to_vec(),
                });
                *logical_size = (*logical_size).max(offset.saturating_add(data.len() as u64));
                if let Some(t) = *truncate_to {
                    *truncate_to = Some(t.max(offset.saturating_add(data.len() as u64)));
                }
                Ok(data.len())
            }
        }
    }

    pub fn copy_from(
        &mut self,
        offset_in: u64,
        len: u64,
        root: &Path,
        daemon: &mut SyncClient,
    ) -> Result<Vec<u8>, libc::c_int> {
        match &mut self.state {
            FileState::Passthrough { file } => {
                if let Err(e) = file.seek(SeekFrom::Start(offset_in)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                let mut buf = vec![0u8; len as usize];
                match file.read(&mut buf) {
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            FileState::CowDirty { tmp, .. }
            | FileState::FuseOnlyDirty { tmp, .. }
            | FileState::FuseOnlyNew { tmp } => {
                let mut f = tmp_as_file(tmp);
                if let Err(e) = f.seek(SeekFrom::Start(offset_in)) {
                    return Err(e.raw_os_error().unwrap_or(libc::EIO));
                }
                let mut buf = vec![0u8; len as usize];
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
                            let start = offset_in as usize;
                            let end = (offset_in as usize + len as usize).min(bytes.len());
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
                            if let Err(e) = f.seek(SeekFrom::Start(offset_in)) {
                                return Err(e.raw_os_error().unwrap_or(libc::EIO));
                            }
                            let mut buf = vec![0u8; len as usize];
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
                        let start = offset_in as usize;
                        let end = (offset_in as usize + len as usize).min(bytes.len());
                        if start >= bytes.len() {
                            Ok(vec![])
                        } else {
                            Ok(bytes[start..end].to_vec())
                        }
                    }
                    Err(_) => Err(libc::EIO),
                }
            }
            FileState::FuseOnlyDirtyRanged { .. } => {
                self.read_at(offset_in, len as u32, root, daemon)
            }
        }
    }

    pub fn set_ranged_size(&mut self, size: u64) {
        if let FileState::FuseOnlyDirtyRanged {
            truncate_to,
            logical_size,
            ..
        } = &mut self.state
        {
            *truncate_to = Some(size);
            *logical_size = size;
        }
    }
}
