use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// Bidirectional map between FUSE inode numbers and absolute filesystem paths.
///
/// Inode numbers are local to a single FUSE daemon session; they are assigned
/// from scratch on each mount and never persisted.  The root of the mounted
/// tree is always inode 1.
pub struct InodeTable {
    ino_to_path: DashMap<u64, PathBuf>,
    path_to_ino: DashMap<PathBuf, u64>,
    next_ino: AtomicU64,
}

impl InodeTable {
    /// Create a new table and pre-insert the root inode (1 → `root`).
    pub fn new(root: PathBuf) -> Self {
        debug_assert!(root.is_absolute(), "root must be an absolute path");
        let t = InodeTable {
            ino_to_path: DashMap::new(),
            path_to_ino: DashMap::new(),
            next_ino: AtomicU64::new(2), // 1 is reserved for root
        };
        t.ino_to_path.insert(1, root.clone());
        t.path_to_ino.insert(root, 1);
        t
    }

    /// Return the existing inode for `path`, or assign a new one and return it.
    ///
    /// `path` must be an absolute path; a `debug_assert` enforces this in
    /// debug builds.
    pub fn get_or_insert(&self, path: &Path) -> u64 {
        debug_assert!(path.is_absolute(), "path must be absolute: {:?}", path);
        if let Some(ino) = self.path_to_ino.get(path) {
            return *ino;
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        self.ino_to_path.insert(ino, path.to_path_buf());
        self.path_to_ino.insert(path.to_path_buf(), ino);
        ino
    }

    /// Look up the absolute path for an inode number, if it exists.
    pub fn get_path(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.get(&ino).map(|p| p.clone())
    }

    /// Look up the inode number for a path, if it has been registered.
    pub fn get_ino(&self, path: &Path) -> Option<u64> {
        self.path_to_ino.get(path).map(|v| *v)
    }
}
