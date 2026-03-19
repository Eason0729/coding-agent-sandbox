use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::syncing::client::SyncClient;

pub const TTL: Duration = Duration::from_secs(1);

pub enum FileState {
    PassthroughReal { file: File },
    PassthroughObject { file: File, object_id: u64 },
}

pub struct OpenFile {
    pub ino: u64,
    pub state: FileState,
}

impl OpenFile {
    pub fn flush_to_daemon(&mut self, _daemon: &mut SyncClient) -> Result<()> {
        match &mut self.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => {
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
        let file = match &mut self.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => file,
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
        let file = match &mut self.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => file,
        };
        file.seek(SeekFrom::Start(offset)).map_err(Error::from)?;
        file.write(data).map_err(Error::from)
    }

    pub fn copy_from(
        &mut self,
        offset_in: u64,
        len: u64,
        _root: &std::path::Path,
        _daemon: &mut SyncClient,
    ) -> Result<Vec<u8>> {
        let file = match &mut self.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => file,
        };
        file.seek(SeekFrom::Start(offset_in)).map_err(Error::from)?;
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).map_err(Error::from)?;
        Ok(buf[..n].to_vec())
    }

    pub fn set_ranged_size(&mut self, size: u64) {
        let file = match &mut self.state {
            FileState::PassthroughReal { file } | FileState::PassthroughObject { file, .. } => file,
        };
        let _ = file.set_len(size);
    }
}
