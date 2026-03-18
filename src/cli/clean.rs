use std::fs;
use std::path::Path;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CleanError {
    #[error("failed to load metadata: {0}")]
    Meta(#[from] crate::syncing::disk::DiskError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn cmd_clean(project_root: &Path) -> Result<(), CleanError> {
    let sandbox_dir = project_root.join(".sandbox");

    if !sandbox_dir.exists() {
        fs::create_dir_all(sandbox_dir.join("data").join("objects"))?;

        let suffix: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(12)
            .map(char::from)
            .collect();
        let shm_name = format!("/cas-{}", suffix);

        crate::syncing::disk::init_sandbox(&project_root.to_path_buf(), &shm_name)?;

        fs::write(sandbox_dir.join("data").join("access.log"), "")?;

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
        return Ok(());
    }

    let meta_result = crate::syncing::disk::load(&project_root.to_path_buf());

    let data_dir = sandbox_dir.join("data");
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir)?;
        println!("Removed {}", data_dir.display());
    }

    if let Ok((meta, _)) = meta_result {
        if !meta.shm_name.is_empty() {
            if let Ok(shm) = crate::shm::ShmState::open(&meta.shm_name) {
                drop(shm);
                let _ = crate::shm::ShmRegion::unlink(&meta.shm_name);
                println!("Reset SHM segment: {}", meta.shm_name);
            }
        }
    }

    let sock_path = sandbox_dir.join("daemon.sock");
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }

    fs::create_dir_all(sandbox_dir.join("data").join("objects"))?;

    let suffix: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    let shm_name = format!("/cas-{}", suffix);

    crate::syncing::disk::init_sandbox(&project_root.to_path_buf(), &shm_name)?;

    fs::write(sandbox_dir.join("data").join("access.log"), "")?;

    println!("Clean complete.");
    Ok(())
}
