Implement `fuse/inode.rs` — `InodeTable`, a bidirectional map between FUSE inode numbers and filesystem paths.

## Struct

- ino_to_path: DashMap<u64, PathBuf>
- path_to_ino: DashMap<PathBuf, u64>
- next_ino: AtomicU64

The table is concurrent and safe for multi-threaded FUSE request handling.


## Notes

- PathBuf is absolute path(a debug_assert here), caller should ensure it is a absolute path.
- Inodes are local to each FUSE daemon session and are never persisted. They are re-assigned from scratch on each mount.
- `get_or_insert` is called from `lookup` every time the kernel resolves a path component; the same path always yields the same ino within a session.
- Entries are never evicted mid-session (the `forget` FUSE call decrements the kernel's nlookup counter but the table entry is kept for simplicity).
