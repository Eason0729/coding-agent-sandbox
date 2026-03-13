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

## Derive Bounds

All types derive `serde::Serialize + serde::Deserialize` and `Debug`.
