use std::fs;
use std::path::Path;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InitError {
    #[error(".sandbox/ already exists — run `cas clean` first")]
    AlreadyExists,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to write metadata: {0}")]
    Meta(#[from] crate::syncing::disk::DiskError),
}

pub fn cmd_init(project_root: &Path) -> Result<(), InitError> {
    let sandbox_dir = project_root.join(".sandbox");

    if sandbox_dir.exists() {
        return Err(InitError::AlreadyExists);
    }

    // Create .sandbox/data/objects/
    fs::create_dir_all(sandbox_dir.join("data").join("objects"))?;

    // Generate random shm_name: "cas-" + 12 alphanumeric chars
    let suffix: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    let shm_name = format!("/cas-{}", suffix);

    // Write metadata.bin and data.bin via init_sandbox
    crate::syncing::disk::init_sandbox(&project_root.to_path_buf(), &shm_name)?;

    // Create empty access.log (server expects it at .sandbox/data/access.log)
    fs::write(sandbox_dir.join("data").join("access.log"), "")?;

    // Write default config.toml (empty lists)
    fs::write(
        sandbox_dir.join("config.toml"),
        include_str!("../../assets/default_conf.toml"),
    )?;

    fs::write(
        sandbox_dir.join(".gitignore"),
        include_str!("../../assets/sandbox_gitignore"),
    )?;

    println!("Initialized sandbox at {}", sandbox_dir.display());
    println!("SHM name: {}", shm_name);

    Ok(())
}
