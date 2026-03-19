use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ObjectError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Object not found: {0}")]
    NotFound(u64),
}

pub struct ObjectStore {
    dir: PathBuf,
    next_id: AtomicU64,
}

impl ObjectStore {
    pub fn new(dir: PathBuf, next_id: u64) -> Self {
        Self {
            dir,
            next_id: AtomicU64::new(next_id),
        }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::Acquire)
    }

    pub fn set_next_id(&self, id: u64) {
        self.next_id.store(id, Ordering::Release);
    }

    pub fn put(&self, data: &[u8]) -> Result<u64, ObjectError> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);

        let path = self.object_path(id);
        let mut file = File::create(&path)?;
        file.write_all(data)?;
        file.sync_all()?;

        Ok(id)
    }

    pub fn alloc_empty(&self) -> Result<u64, ObjectError> {
        self.put(&[])
    }

    pub fn path_for(&self, id: u64) -> PathBuf {
        self.object_path(id)
    }

    pub fn get(&self, id: u64) -> Result<Vec<u8>, ObjectError> {
        let path = self.object_path(id);
        if !path.exists() {
            return Err(ObjectError::NotFound(id));
        }
        let mut file = File::open(&path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(data)
    }

    pub fn get_range(&self, id: u64, offset: u64, len: usize) -> Result<Vec<u8>, ObjectError> {
        let path = self.object_path(id);
        if !path.exists() {
            return Err(ObjectError::NotFound(id));
        }
        let mut file = File::open(&path)?;
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut data = vec![0u8; len];
        let n = file.read(&mut data)?;
        data.truncate(n);
        Ok(data)
    }

    pub fn exists(&self, id: u64) -> bool {
        self.object_path(id).exists()
    }

    fn object_path(&self, id: u64) -> PathBuf {
        let hex = format!("{:016x}", id);
        let shard = format!("{:02x}", id & 0xff);
        self.dir.join(shard).join(hex)
    }

    pub fn init_dir(dir: &PathBuf) -> Result<(), ObjectError> {
        for i in 0..=0xff {
            let subdir = dir.join(format!("{:02x}", i));
            fs::create_dir_all(&subdir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_sharding_distributes_ids() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        ObjectStore::init_dir(&dir).unwrap();

        let mut store = ObjectStore::new(dir, 1);

        let mut shards = std::collections::HashSet::new();
        for _ in 0..512 {
            let id = store.put(b"test").unwrap();
            shards.insert(id & 0xff);
        }

        assert!(
            shards.len() > 1,
            "Expected distribution across multiple shards, got {}",
            shards.len()
        );
    }

    #[test]
    fn test_object_path_low_byte_shard() {
        let tmp = TempDir::new().unwrap();
        let store = ObjectStore::new(tmp.path().to_path_buf(), 0);

        assert_eq!(
            store.object_path(0),
            tmp.path().join("00").join("0000000000000000")
        );
        assert_eq!(
            store.object_path(1),
            tmp.path().join("01").join("0000000000000001")
        );
        assert_eq!(
            store.object_path(255),
            tmp.path().join("ff").join("00000000000000ff")
        );
        assert_eq!(
            store.object_path(256),
            tmp.path().join("00").join("0000000000000100")
        );
        assert_eq!(
            store.object_path(0x0001_00ff),
            tmp.path().join("ff").join("00000000000100ff")
        );
    }
}
