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

## Request/response list

| Intent | Request | Response |
|---|---|---|
| ensure regular file has backing object | `EnsureFileObject { path, meta }` | `EnsureFileObject { id, path }` |
| map object id to real object path | `GetObjectPath { id }` | `GetObjectPath { path }` or `NotFound` |
| explicit file entry upsert | `UpsertFileEntry { path, object_id, meta }` | `UpsertFileEntry` |
| update metadata only | `PutFileMeta { path, meta }` | `PutFileMeta` |
| fetch metadata only | `GetFileMeta { path }` | `GetFileMeta(Option<FileMetadata>)` |
| lookup exact path entry | `GetEntry { path }` | `GetEntry(Option<FuseEntry>)` |
| remove exact path entry | `DeleteFile { path }` | `DeleteFile` |
| rename exact path | `RenameFile { from, to }` | `RenameFile` |
| upsert directory entry | `PutDir { path, meta }` | `PutDir` |
| upsert symlink entry | `PutSymlink { path, target, meta }` | `PutSymlink` |
| place exact-path tombstone | `PutWhiteout { path }` | `PutWhiteout` |
| remove exact-path tombstone | `DeleteWhiteout { path }` | `DeleteWhiteout` |
| list direct children entries | `ReadDirAll { path }` | `DirEntries(Vec<(PathBuf, FuseEntry)>)` |
| list descendant whiteouts | `ListWhiteoutUnder { path }` | `WhiteoutPaths(Vec<PathBuf>)` |
| rename subtree entries | `RenameTree { from, to }` | `RenameTree` |
| append access log | `LogAccess { path, operation, pid }` | `LogAccess` |
| persist daemon state | `Flush` | `Flush` |

## Merge/whiteout semantics

- Whiteout is exact-path hide marker.
- A managed directory does not implicitly hide its subtree.
- Readdir merge is done in FUSE layer; server only returns direct children entries.

## Derive bounds

All protocol types derive `serde::Serialize + serde::Deserialize + Debug`.
