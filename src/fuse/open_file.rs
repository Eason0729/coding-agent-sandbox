use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::syncing::client::SyncClient;

pub const TTL: Duration = Duration::from_secs(1);

fn write_all_at(file: &mut File, offset: u64, data: &[u8]) -> Result<usize> {
    file.seek(SeekFrom::Start(offset)).map_err(Error::from)?;
    file.write_all(data).map_err(Error::from)?;
    Ok(data.len())
}

pub enum OpenFile {
    PassthroughReal { file: File },
    PassthroughObject { file: File, object_id: u64 },
}

impl AsRef<File> for OpenFile {
    fn as_ref(&self) -> &File {
        match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        }
    }
}

impl AsMut<File> for OpenFile {
    fn as_mut(&mut self) -> &mut File {
        match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        }
    }
}

impl OpenFile {
    pub fn flush_to_daemon(&mut self, _daemon: &mut SyncClient) -> Result<()> {
        match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => {
                file.sync_data().map_err(Error::from)
            }
        }
    }

    pub fn read_at(
        &mut self,
        offset: u64,
        size: u32,
        _root: &std::path::Path,
        _daemon: &mut SyncClient,
    ) -> Result<Vec<u8>> {
        let file = match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        };
        file.seek(SeekFrom::Start(offset)).map_err(Error::from)?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read(&mut buf).map_err(Error::from)?;
        Ok(buf[..n].to_vec())
    }

    pub fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
        _root: &std::path::Path,
        _daemon: &mut SyncClient,
    ) -> Result<usize> {
        let file = match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        };
        write_all_at(file, offset, data)
    }

    pub fn copy_from(
        &mut self,
        offset_in: u64,
        len: u64,
        _root: &std::path::Path,
        _daemon: &mut SyncClient,
    ) -> Result<Vec<u8>> {
        let file = match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        };
        file.seek(SeekFrom::Start(offset_in)).map_err(Error::from)?;
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).map_err(Error::from)?;
        Ok(buf[..n].to_vec())
    }

    pub fn set_ranged_size(&mut self, size: u64) {
        let file = match self {
            OpenFile::PassthroughReal { file } | OpenFile::PassthroughObject { file, .. } => file,
        };
        let _ = file.set_len(size);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use tempfile::tempdir;

    #[test]
    fn write_at_persists_full_buffer() {
        let tempdir = tempdir().expect("create temp dir");
        let path = tempdir.path().join("file.bin");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("open temp file");
        let payload = vec![0xAB; 1024 * 1024 + 17];

        let written = super::write_all_at(&mut file, 0, &payload).expect("write_all_at succeeds");
        assert_eq!(written, payload.len());

        let mut verify = std::fs::File::open(&path).expect("reopen file");
        verify.seek(SeekFrom::Start(0)).expect("seek verify file");
        let mut buf = Vec::new();
        verify.read_to_end(&mut buf).expect("read verify file");
        assert_eq!(buf.len(), payload.len());
        assert_eq!(buf, payload);
    }
}
