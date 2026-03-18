use std::path::Path;

pub fn cmd_init(project_root: &Path) -> Result<(), crate::cli::clean::CleanError> {
    crate::cli::cmd_clean(project_root)
}
