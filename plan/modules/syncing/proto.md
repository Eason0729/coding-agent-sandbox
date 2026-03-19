Implement `syncing/proto.rs` as a metadata-only protocol between FUSE daemons and the syncing daemon.

## Design goal

Protocol never transfers file content bytes. It only manages:

- path -> metadata map
- path -> object id mapping for regular files
- object id -> object path lookup for FUSE to open backing files directly on real FS

Object file content is written/read by FUSE via normal filesystem syscalls.

## Core data types

```rust
struct FileMetadata {
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    atime: u64,
    ctime: u64,
}

enum EntryType {
    File,
    Dir,
    Symlink,
    Whiteout,
}

struct FuseEntry {
    entry_type: EntryType,
    metadata: FileMetadata,
    object_id: Option<u64>,
    symlink_target: Option<Vec<u8>>,
}
```
