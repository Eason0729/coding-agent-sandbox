Implement `syncing/proto.rs` — shared data types for serialization and the request/response protocol between FUSE daemons and the syncing daemon.

## List of all message/response

| Intent | Request sent | Expected response |
|---|---|---|
| `put_object(&mut self, data: &[u8]) -> Result<u64>` | `PutObject` | `PutObject { id }` |
| `get_object(&mut self, id: u64) -> Result<Vec<u8>>` | `GetObject` | `GetObject { data }` |
| `put_file(&mut self, path: PathBuf, data: Vec<u8>, meta: FileMetadata) -> Result<u64>` | `PutFile` | `PutFile { id }` |
| `put_file_meta(&mut self, path: PathBuf, meta: FileMetadata) -> Result<()>` | `PutFileMeta` | `Ok` |
| `get_file_meta(&mut self, path: PathBuf) -> Result<Option<FileMetadata>>` | `GetFileMeta` | `FileMeta` or `NotFound → None` |
| `get_entry(&mut self, path: PathBuf) -> Result<Option<FuseEntry>>` | `GetEntry` | `Entry` or `NotFound` |
| `delete_file(&mut self, path: PathBuf) -> Result<()>` | `DeleteFile` | `Ok` |
| `rename_file(&mut self, from: PathBuf, to: PathBuf) -> Result<()>` | `RenameFile` | `Ok` |
| `put_dir(&mut self, path: PathBuf, meta: DirMetadata) -> Result<()>` | `PutDir` | `Ok` |
| `put_whiteout(&mut self, path: PathBuf) -> Result<()>` | `PutWhiteout` | `Ok` |
| `read_dir_all(&mut self, path: PathBuf) -> Result<Vec<(PathBuf, FuseEntry)>>` | `ReadDirAll` | `DirEntries` |
| `log_access(&mut self, path: PathBuf, operation: String, pid: u32) -> Result<()>` | `LogAccess` | `Ok` |
| `flush(&mut self) -> Result<()>` | `Flush` | `Ok` |

## Protocol v2 (breaking)

`PutObject`/`GetObject` remain for full blob operations, but v2 canonical data path is ranged:

| Intent | Request sent | Expected response |
|---|---|---|
| `get_object_range(&mut self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>>` | `GetObjectRange` | `GetObjectRange { data }` |
| `patch_file(&mut self, path: PathBuf, patches: Vec<BytePatch>, truncate_to: Option<u64>, meta: FileMetadata) -> Result<u64>` | `PatchFile` | `PatchFile { id }` |

### BytePatch

`BytePatch` is an absolute in-file update:

```rust
struct BytePatch {
    offset: u64,
    data: Vec<u8>,
}
```

Patches are applied in request order. Overlapping ranges are allowed; later patches overwrite earlier bytes.

## Derive Bounds

All types derive `serde::Serialize + serde::Deserialize` and `Debug`.
