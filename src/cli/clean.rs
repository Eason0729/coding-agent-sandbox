use std::fs;
use std::path::Path;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CleanError {
    #[error("not initialized — run `cas init` first")]
    NotInitialized,
    #[error("failed to load metadata: {0}")]
    Meta(#[from] crate::syncing::disk::DiskError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn cmd_clean(project_root: &Path) -> Result<(), CleanError> {
    let sandbox_dir = project_root.join(".sandbox");

    if !sandbox_dir.exists() {
        return Err(CleanError::NotInitialized);
    }

    // Load metadata to get the shm_name before we delete data
    let meta_result = crate::syncing::disk::load(&project_root.to_path_buf());

    // Remove .sandbox/data/ (FUSE data and objects)
    let data_dir = sandbox_dir.join("data");
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir)?;
        println!("Removed {}", data_dir.display());
    }

    // Reset SHM counter if we can find the segment
    if let Ok((meta, _)) = meta_result {
        if !meta.shm_name.is_empty() {
            // Try to open and reset, ignore errors (segment may not exist)
            if let Ok(shm) = crate::shm::ShmState::open(&meta.shm_name) {
                // Reset running_count to 0 by re-creating
                drop(shm);
                let _ = crate::shm::ShmRegion::unlink(&meta.shm_name);
                println!("Reset SHM segment: {}", meta.shm_name);
            }
        }
    }

    // Remove daemon socket if present
    let sock_path = sandbox_dir.join("daemon.sock");
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }

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

    println!("Clean complete.");
    Ok(())
}
