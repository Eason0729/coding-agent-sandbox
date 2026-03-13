use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bidirectional map between FUSE inode numbers and absolute filesystem paths.
///
/// Inode numbers are local to a single FUSE daemon session; they are assigned
/// from scratch on each mount and never persisted.  The root of the mounted
/// tree is always inode 1.
pub struct InodeTable {
    ino_to_path: BTreeMap<u64, PathBuf>,
    path_to_ino: BTreeMap<PathBuf, u64>,
    next_ino: u64,
}

impl InodeTable {
    /// Create a new table and pre-insert the root inode (1 → `root`).
    pub fn new(root: PathBuf) -> Self {
        debug_assert!(root.is_absolute(), "root must be an absolute path");
        let mut t = InodeTable {
            ino_to_path: BTreeMap::new(),
            path_to_ino: BTreeMap::new(),
            next_ino: 2, // 1 is reserved for root
        };
        t.ino_to_path.insert(1, root.clone());
        t.path_to_ino.insert(root, 1);
        t
    }

    /// Return the existing inode for `path`, or assign a new one and return it.
    ///
    /// `path` must be an absolute path; a `debug_assert` enforces this in
    /// debug builds.
    pub fn get_or_insert(&mut self, path: &Path) -> u64 {
        debug_assert!(path.is_absolute(), "path must be absolute: {:?}", path);
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_path_buf());
        self.path_to_ino.insert(path.to_path_buf(), ino);
        ino
    }

    /// Look up the absolute path for an inode number, if it exists.
    pub fn get_path(&self, ino: u64) -> Option<&Path> {
        self.ino_to_path.get(&ino).map(PathBuf::as_path)
    }

    /// Look up the inode number for a path, if it has been registered.
    pub fn get_ino(&self, path: &Path) -> Option<u64> {
        self.path_to_ino.get(path).copied()
    }
}
