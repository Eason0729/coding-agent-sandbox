use std::path::Path;

/// How the FUSE layer handles reads and writes for a given path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessMode {
    /// Always read/write on the real filesystem (pass-through).
    Passthrough,
    /// Always read/write on the FUSE layer only (no real-FS access).
    FuseOnly,
    /// Copy-on-write: if data exists in the FUSE store, use it; otherwise
    /// provide a read view of the real FS and copy-on-write into the FUSE store.
    CopyOnWrite,
}

/// Per-path policy used by `CasFuseFs` to decide how to route operations.
pub trait Policy: Send + Sync + 'static {
    /// Classify the access mode for the given (absolute) path.
    fn classify(&self, path: &Path) -> AccessMode;

    /// Whether first-access to the given path should be logged.
    fn should_log(&self, path: &Path) -> bool;
}
