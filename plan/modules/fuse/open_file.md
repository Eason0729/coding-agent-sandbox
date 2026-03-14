# fuse/open_file.rs — Open File State and Operations

## Overview

`open_file.rs` encapsulates all state and operations for files opened through the FUSE layer. It extracts the `FileState` enum, `OpenFile` struct, and their associated methods from `fs.rs`.

## FileState Enum

```rust
enum FileState {
    /// Passthrough: we hold a real `File` handle on the underlying FS.
    Passthrough { file: File },
    /// CoW, not yet written: authoritative content is still on the real FS or
    /// the daemon store. `object_id` is `Some(id)` when the daemon has a
    /// stored version, `None` when the real-FS file is authoritative.
    CowClean { object_id: Option<u64> },
    /// CoW, already written: we hold a `NamedTempFile` with the dirty bytes.
    /// `object_id_before` records the previous daemon object (for reference).
    CowDirty {
        tmp: NamedTempFile,
        object_id_before: Option<u64>,
    },
    /// FuseOnly file that doesn't exist in the store yet (newly created).
    FuseOnlyNew { tmp: NamedTempFile },
    /// FuseOnly file backed by a daemon object.
    FuseOnlyClean { object_id: u64 },
    /// FuseOnly file with dirty data in a temp file.
    FuseOnlyDirty { tmp: NamedTempFile, object_id: u64 },
}
```

## OpenFile Struct

```rust
struct OpenFile {
    path: PathBuf,
    state: FileState,
}
```

## State Transitions

```
                    +------------------+
                    |   CowClean       |
                    | (no temp file)  |
                    +--------+---------+
                             |
                  first write|
                  or truncate|
                             v
                    +------------------+
          +-------->|   CowDirty       |
          |        | (temp file with |
          |        |  dirty content)  |
          |        +------------------+
          |
          | materialize (on read with daemon object)
          |
          v
+------------------+    first write
|  FuseOnlyClean  |------------------->+------------------+
| (daemon object) |                     |  FuseOnlyDirty   |
+------------------+                     | (temp file)     |
                                         +------------------+

FuseOnlyNew (created via create/mkdir) ---> never changes (stays in open_files until close)
Passthrough --------------------------------> never changes
```

## Operations on OpenFile

### materialize(&mut self, root: &Path, daemon: &mut SyncClient) -> Result<(), libc::c_int>

Materialize a `CowClean` file into a `CowDirty` temp file so writes can proceed.

**Algorithm:**
1. If `state` is already `CowDirty` → return `Ok(())` (no-op)
2. If `state` is `CowClean { object_id }`:
   - If `object_id` is `Some(id)`: fetch bytes from daemon via `daemon.get_object(id)`
   - If `object_id` is `None`: read bytes from real FS at `root.join(path)`
   - Write bytes to a new `NamedTempFile`
   - Seek to start
   - Transition state to `CowDirty { tmp, object_id_before: object_id }`
3. For all other states → return `Ok(())` (Passthrough, FuseOnly* handled elsewhere)

**Error:** Returns `libc::EIO` on any I/O failure.

---

### flush_to_daemon(&mut self, path: &Path, daemon: &mut SyncClient) -> Result<(), libc::c_int>

Flush dirty content back to the syncing daemon. Called from both `flush` and `release` handlers.

**Algorithm:**
1. If `state` is `CowDirty { tmp, object_id_before }` or `FuseOnlyDirty { tmp, object_id }`:
   - Read all bytes from temp file
   - Build `FileMetadata` from temp file stat (uid, gid, mode, mtime/atime/ctime = now)
   - Call `daemon.put_file(path, bytes, metadata)`
2. If `state` is `FuseOnlyNew { tmp }`:
   - Read all bytes from temp file
   - Build `FileMetadata` (default uid=0, gid=0, mode=0o644, timestamps=now)
   - Call `daemon.put_file(path, bytes, metadata)`
3. For all other states → return `Ok(())` (no-op)

**Error:** Returns `libc::EIO` on any failure.

---

### read_at(&mut self, offset: u64, size: u32, root: &Path, daemon: &mut SyncClient) -> Result<Vec<u8>, libc::c_int>

Read data from the open file. Handles all 6 state variants.

**Algorithm:**
1. `Passthrough { file }`: seek to offset, read `size` bytes, return
2. `CowDirty { tmp, .. }` / `FuseOnlyDirty { tmp, .. }` / `FuseOnlyNew { tmp }`:
   - Use `tmp_as_file()` helper to get File handle, seek to offset, read
3. `CowClean { object_id }`:
   - If `object_id` is `Some(id)`: fetch from daemon, slice by offset/size, return
   - If `object_id` is `None`: read from real FS at `root.join(path)`, slice, return
4. `FuseOnlyClean { object_id }`: fetch from daemon, slice, return

**Error:** Returns `libc::EIO` on read failures, `libc::ENOENT` if file not found.

---

### write_at(&mut self, offset: u64, data: &[u8], root: &Path, daemon: &mut SyncClient) -> Result<usize, libc::c_int>

Write data to the open file. **Automatically handles materialization and state promotion.**

**Algorithm:**
1. `Passthrough { file }`: seek to offset, write data, return bytes written
2. `CowDirty { tmp, .. }` / `FuseOnlyDirty { tmp, .. }` / `FuseOnlyNew { tmp }`:
   - Use `tmp_as_file()` helper, seek to offset, write, return
3. `CowClean { object_id }`: **auto-materialize first**
   - Call `self.materialize(root, daemon)`
   - Retry write as `CowDirty`
4. `FuseOnlyClean { object_id }`: **promote to dirty first**
   - Fetch current content from daemon into new temp file
   - Write new data at offset to temp file
   - Transition state to `FuseOnlyDirty { tmp, object_id: id }`
   - Return bytes written

**Error:** Returns `libc::EIO` on any failure.

---

## Performance Update (v2)

Introduce a sparse dirty state to avoid full-object rewrite on seek+small-write:

```rust
FileState::FuseOnlyDirtyRanged {
    object_id: u64,
    patches: Vec<BytePatch>,
    truncate_to: Option<u64>,
    logical_size: u64,
}
```

`CowDirty` can continue using tempfile for now, but `FuseOnlyClean` must promote to `FuseOnlyDirtyRanged` on first write.

`read_at` for ranged dirty files composes bytes from:
1. base object range from daemon (`get_object_range`)
2. in-memory patches that overlap requested range (patches override base)

`flush_to_daemon` for ranged dirty files sends `PatchFile` instead of full `put_file`.

---

### tmp_as_file(tmp: &NamedTempFile) -> ManuallyDrop<File>

Helper to get a `File` view of a temp file without taking ownership.

```rust
fn tmp_as_file(tmp: &NamedTempFile) -> ManuallyDrop<File> {
    unsafe { std::mem::ManuallyDrop::new(File::from_raw_fd(tmp.as_raw_fd())) }
}
```

**Why this helper exists:** The same 5-line unsafe pattern appears in `read_at`, `write_at`, `materialize`, and `flush_to_daemon`. Deduplicating ensures consistency and reduces risk of subtle bugs (e.g., forgetting to seek after creating the File view).

---

## Integration with fs.rs

`open_file.rs` exposes a public API:

```rust
pub use self::open_file::{FileState, OpenFile};

impl OpenFile {
    pub fn materialize(&mut self, root: &Path, daemon: &mut SyncClient) -> Result<(), libc::c_int>
    pub fn flush_to_daemon(&mut self, path: &Path, daemon: &mut SyncClient) -> Result<(), libc::c_int>
    pub fn read_at(&mut self, offset: u64, size: u32, root: &Path, daemon: &mut SyncClient) -> Result<Vec<u8>, libc::c_int>
    pub fn write_at(&mut self, offset: u64, data: &[u8], root: &Path, daemon: &mut SyncClient) -> Result<usize, libc::c_int>
}

pub fn tmp_as_file(tmp: &NamedTempFile) -> ManuallyDrop<File>
```

The `fs.rs` handlers call these methods on `OpenFile` instances retrieved from `Inner::open_files`.
