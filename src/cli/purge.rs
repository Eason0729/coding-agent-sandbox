use std::fs;
use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PurgeError {
    #[error("not initialized — run `cas init` first")]
    NotInitialized,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn cmd_purge(project_root: &Path) -> Result<(), PurgeError> {
    let sandbox_dir = project_root.join(".sandbox");

    if !sandbox_dir.exists() {
        return Err(PurgeError::NotInitialized);
    }

    fs::remove_dir_all(&sandbox_dir)?;
    println!("Removed {}.", sandbox_dir.display());
    Ok(())
}
