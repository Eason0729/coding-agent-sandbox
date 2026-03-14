# fuse/fs.rs — FUSE Filesystem Orchestration

## Overview

`fs.rs` is the main FUSE filesystem implementation. It implements the `fuser::Filesystem` trait and orchestrates policy enforcement, inode management, and delegates actual file I/O to `open_file.rs`.

**After refactoring:** `fs.rs` is a thin orchestration layer (~750 lines), delegating heavy lifting to specialized modules.

## Module Structure

```
src/fuse/
├── mod.rs           # Re-exports
├── fs.rs            # CasFuseFs + impl Filesystem (thin handlers)
├── open_file.rs     # FileState, OpenFile, read_at, write_at, materialize, flush
├── attr.rs          # attr_from_meta, attr_from_nix_stat, attr_from_daemon
├── inode.rs         # InodeTable
├── policy.rs        # Policy trait, AccessMode
└── mount.rs         # run_fuse, unmount
```

## Root

`CasFuseFs` is constructed with `root = PathBuf::from("/")`. It presents the **entire host filesystem** to the sandboxed process. The `real_path` helper resolves FUSE paths against this root, so every host path is accessible via FUSE.

The `policy` determines behavior per-path:
- `project_root` is implicitly added to the **whitelist** → **Passthrough**: reads and writes go directly to the real FS.
- `.sandbox/` is implicitly **HideReal** → reads return the fuse store (empty by default → ENOENT), writes go to fuse store. The real `.sandbox` directory is never exposed inside the sandbox.
- Everything else (e.g. `/bin`, `/usr`, `/lib`) falls through to the **default (CopyOnWrite)** tier: reads come from the real FS, writes are redirected to the fuse store, and first access is logged.

This means the sandboxed process sees a complete root filesystem through the FUSE mount; no system directories need to be separately bind-mounted into the chroot.

## Inner Struct (kept in fs.rs)

```rust
struct Inner {
    root: PathBuf,                 // Real FS root path
    inodes: InodeTable,           // Bidirectional inode ↔ path mapping
    open_files: HashMap<u64, OpenFile>,  // Open file handles
    next_fh: u64,                 // Next file-handle number
    daemon: SyncClient,           // Connection to syncing daemon
}
```

**Methods kept in Inner:**
- `path_of(&self, ino: u64) -> Option<&Path>` — resolve inode to path
- `alloc_fh(&mut self) -> u64` — allocate new file handle
- `real_path(&self, path: &Path) -> PathBuf` — resolve FUSE path to real FS path
- `stat_path(&mut self, path: &Path, mode: &AccessMode) -> Result<(FileType, FileAttr), libc::c_int>` — stat with policy-aware fallback

## Thin Handler Pattern

Each FUSE handler follows a consistent pattern:

1. Acquire only the minimal lock needed (inode-table mutex, per-open-file mutex, or daemon client)
2. Resolve inode → path (or return ENOENT)
3. Get `OpenFile` from the concurrent file-handle table (for read/write operations)
4. Delegate to `OpenFile` methods for actual I/O
5. Return result via reply

## Performance Update (v2)

The single global `Arc<Mutex<Inner>>` is replaced by finer-grained synchronization:

- inode table: per-row concurrent hashmap
- open file table: `fh -> Arc<Mutex<OpenFile>>` so file-handle operations serialize per-fh, not globally
- daemon client: bounded `SyncClientPool` checkout per operation, reusing persistent unix-socket connections while avoiding one shared stream bottleneck

Goal: allow independent FUSE operations on distinct files/inodes to proceed concurrently.

**Example: read handler (~15 lines)**

```rust
fn read(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, size: u32, ...) {
    let of_arc = match self.open_files.get(&fh.0) {
        Some(v) => Arc::clone(v.value()),
        None => { reply.error(errno(libc::EBADF)); return; }
    };
    if self.inodes.lock().unwrap().get_path(ino.0).is_none() {
        reply.error(errno(libc::ENOENT));
        return;
    }
    let mut daemon = match self.connect_daemon() {
        Ok(d) => d,
        Err(code) => { reply.error(errno(code)); return; }
    };
    let mut of = of_arc.lock().unwrap();
    match of.read_at(offset, size, &self.root, &mut daemon) {
        Ok(buf) => reply.data(&buf),
        Err(code) => reply.error(errno(code)),
    }
}
```

**Example: write handler (~15 lines)**

```rust
fn write(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], ...) {
    let of_arc = match self.open_files.get(&fh.0) {
        Some(v) => Arc::clone(v.value()),
        None => { reply.error(errno(libc::EBADF)); return; }
    };
    if self.inodes.lock().unwrap().get_path(ino.0).is_none() {
        reply.error(errno(libc::ENOENT));
        return;
    }
    let mut daemon = match self.connect_daemon() {
        Ok(d) => d,
        Err(code) => { reply.error(errno(code)); return; }
    };
    let mut of = of_arc.lock().unwrap();
    match of.write_at(offset, data, &self.root, &mut daemon) {
        Ok(n) => reply.written(n as u32),
        Err(code) => reply.error(errno(code)),
    }
}
```

## Helper: resolve_ino

To reduce boilerplate, extract a helper method:

```rust
impl Inner {
    fn resolve_ino(&self, ino: INodeNo) -> Option<(INodeNo, PathBuf)> {
        self.inodes.get_path(ino.0).map(|p| (ino, p.to_path_buf()))
    }
}
```

Usage in handlers:
```rust
let (_, path) = match g.resolve_ino(ino) {
    Some(x) => x,
    None => { reply.error(errno(libc::ENOENT)); return; }
};
```

## Handlers and Their Delegation

| Handler | Delegates To | Notes |
|---|---|---|
| `lookup` | `stat_path()` | Returns entry via reply |
| `getattr` | `stat_path()` | Returns attr via reply |
| `setattr` | Two helpers: `setattr_passthrough()`, `setattr_fuse_or_cow()` | Complex, stays in fs.rs |
| `readlink` | Direct: Passthrough → real FS, FuseOnly → daemon | Stays in fs.rs |
| `open` | `OpenFile::new()` with appropriate state | Returns fh |
| `create` | Direct: Passthrough → real FS, FuseOnly/CoW → daemon | Returns fh + attr |
| `read` | `of.read_at()` | Thin wrapper |
| `write` | `of.write_at()` | Thin wrapper (auto-materializes) |
| `flush` | `of.flush_to_daemon()` | Thin wrapper |
| `release` | `of.flush_to_daemon()` if flush=true | Thin wrapper |
| `fsync` | `of.flush_to_daemon()` | Thin wrapper |
| `mkdir` | Direct: Passthrough → real FS, FuseOnly/CoW → daemon | Stays in fs.rs |
| `unlink` | Direct: Passthrough → real FS, CoW → whiteout, FuseOnly → daemon | Stays in fs.rs |
| `rmdir` | Direct: Passthrough → real FS, FuseOnly/CoW → daemon | Stays in fs.rs |
| `rename` | Direct: same-policy → real FS or daemon | Stays in fs.rs |
| `symlink` | Direct: Passthrough → real FS, FuseOnly/CoW → daemon | Stays in fs.rs |
| `link` | Returns EPERM (v1 non-goal) | Stays in fs.rs |
| `opendir` | Check inode exists | Thin |
| `readdir` | Direct: merge real + daemon entries | Complex, stays in fs.rs |
| `releasedir` | No-op | Thin |
| `statfs` | Direct: `nix::sys::statvfs::statvfs(root)` | Stays in fs.rs |
| `access` | Check inode exists | Thin |
| `getxattr`... | Return ENOTSUP | Stubs |

## Edge Cases

### MMap

When `mmap` is involved, materialization must happen early. The kernel may call `open` without `O_WRONLY`, then later `mmap` with `PROT_WRITE`, which triggers a page fault → `write` call. The `write_at` method handles this automatically by materializing on first write.

### setattr Complexity

`setattr` handles three scenarios:
1. **Passthrough**: apply to real FS via `fs::OpenOptions`, `fs::Permissions`, `utimensat`
2. **FuseOnly/CoW with open fh**: apply to temp file via `ftruncate`, then stat to get new attr
3. **FuseOnly/CoW without open fh**: apply to daemon metadata via `put_file_meta`

This complexity stays in `fs.rs` as `setattr_passthrough()` and `setattr_fuse_or_cow()` helper methods on `Inner`.

## What Was Extracted

The following moved to `open_file.rs`:
- `FileState` enum
- `OpenFile` struct
- `materialize()` method
- `flush_to_daemon()` method
- `read_at()` method (with all 6 state variants)
- `write_at()` method (with auto-materialization)
- `tmp_as_file()` helper

The following moved to `attr.rs`:
- `attr_from_meta()`
- `attr_from_nix_stat()`
- `attr_from_daemon()`

## FUSE Operation Implementations

Almost every operation need to be supported.

Here we document some edge case where agent might fall:

### MMap

Materialize early when mmap is involved
