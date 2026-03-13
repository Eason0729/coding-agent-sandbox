use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

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
    next_id: u64,
}

impl ObjectStore {
    pub fn new(dir: PathBuf, next_id: u64) -> Self {
        Self { dir, next_id }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    pub fn set_next_id(&mut self, id: u64) {
        self.next_id = id;
    }

    pub fn put(&mut self, data: &[u8]) -> Result<u64, ObjectError> {
        let id = self.next_id;
        self.next_id += 1;

        let path = self.object_path(id);
        let mut file = File::create(&path)?;
        file.write_all(data)?;
        file.sync_all()?;

        Ok(id)
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

    pub fn exists(&self, id: u64) -> bool {
        self.object_path(id).exists()
    }

    fn object_path(&self, id: u64) -> PathBuf {
        let hex = format!("{:016x}", id);
        let prefix = &hex[0..2];
        self.dir.join(prefix).join(hex)
    }

    pub fn init_dir(dir: &PathBuf) -> Result<(), ObjectError> {
        for i in 0..=0xff {
            let subdir = dir.join(format!("{:02x}", i));
            fs::create_dir_all(&subdir)?;
        }
        Ok(())
    }
}
