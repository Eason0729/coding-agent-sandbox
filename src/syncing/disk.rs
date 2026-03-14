use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::syncing::proto::FuseEntry;

#[derive(Error, Debug)]
pub enum DiskError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("Object error: {0}")]
    Object(#[from] crate::syncing::object::ObjectError),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SandboxMeta {
    pub shm_name: String,
    pub abi_version: u32,
    pub next_id: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FuseMap {
    pub entries: HashMap<PathBuf, FuseEntry>,
}

pub struct AccessLog {
    file: File,
}

impl AccessLog {
    pub fn open(path: &PathBuf) -> Result<Self, DiskError> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    pub fn log(&mut self, path: &PathBuf, operation: &str, pid: u32) -> Result<(), DiskError> {
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        writeln!(
            self.file,
            "[{}] {} {} {}",
            timestamp,
            pid,
            operation,
            path.display()
        )?;
        self.file.flush()?;
        Ok(())
    }
}

pub fn load(sandbox_dir: &PathBuf) -> Result<(SandboxMeta, FuseMap), DiskError> {
    let meta_path = sandbox_dir
        .join(".sandbox")
        .join("data")
        .join("metadata.bin");
    let map_path = sandbox_dir.join(".sandbox").join("data").join("data.bin");

    let meta = if meta_path.exists() {
        let mut file = File::open(&meta_path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        postcard::from_bytes(&data)?
    } else {
        SandboxMeta {
            shm_name: String::new(),
            abi_version: 2,
            next_id: 1,
        }
    };

    let fuse_map = if map_path.exists() {
        let mut file = File::open(&map_path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        postcard::from_bytes(&data)?
    } else {
        FuseMap::default()
    };

    Ok((meta, fuse_map))
}

pub fn flush(
    sandbox_dir: &PathBuf,
    meta: &SandboxMeta,
    fuse_map: &FuseMap,
) -> Result<(), DiskError> {
    let meta_path = sandbox_dir
        .join(".sandbox")
        .join("data")
        .join("metadata.bin");
    let map_path = sandbox_dir.join(".sandbox").join("data").join("data.bin");

    let meta_data = postcard::to_allocvec(meta)?;
    let mut meta_file = File::create(&meta_path)?;
    meta_file.write_all(&meta_data)?;
    meta_file.sync_all()?;

    let map_data = postcard::to_allocvec(fuse_map)?;
    let mut map_file = File::create(&map_path)?;
    map_file.write_all(&map_data)?;
    map_file.sync_all()?;

    Ok(())
}

pub fn init_sandbox(sandbox_dir: &PathBuf, shm_name: &str) -> Result<(), DiskError> {
    let sandbox_data_dir = sandbox_dir.join(".sandbox").join("data");
    fs::create_dir_all(&sandbox_data_dir)?;
    fs::create_dir_all(sandbox_data_dir.join("objects"))?;

    let objects_dir = sandbox_data_dir.join("objects");
    for i in 0..=0xff {
        let subdir = objects_dir.join(format!("{:02x}", i));
        fs::create_dir_all(&subdir)?;
    }

    let meta = SandboxMeta {
        shm_name: shm_name.to_string(),
        abi_version: 2,
        next_id: 1,
    };
    let fuse_map = FuseMap::default();
    flush(sandbox_dir, &meta, &fuse_map)?;

    Ok(())
}
