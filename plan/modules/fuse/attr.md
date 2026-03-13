# fuse/attr.rs — File Attribute Builders

## Overview

`attr.rs` contains pure functions that build FUSE `FileAttr` structs from various metadata sources. These are extracted from `fs.rs` to improve modularity and testability.

## FileAttr Fields

The FUSE `FileAttr` struct contains:

| Field | Type | Description |
|---|---|---|
| `ino` | `INodeNo` | Inode number |
| `size` | `u64` | File size in bytes |
| `blocks` | `u64` | Number of 512-byte blocks allocated |
| `atime` | `SystemTime` | Last access time |
| `mtime` | `SystemTime` | Last modification time |
| `ctime` | `SystemTime` | Last metadata change time |
| `crtime` | `SystemTime` | Creation time (birth time) |
| `kind` | `FileType` | File type (RegularFile, Directory, Symlink) |
| `perm` | `u16` | Permission bits (mode & 0o7777) |
| `nlink` | `u32` | Number of hard links |
| `uid` | `u32` | Owner user ID |
| `gid` | `u32` | Owner group ID |
| `rdev` | `u32` | Device ID (for special files) |
| `blksize` | `u32` | Block size for I/O |
| `flags` | `u32` | File flags |

## Builder Functions

### attr_from_meta(ino: u64, meta: &std::fs::Metadata) -> FileAttr

Build `FileAttr` from real filesystem metadata.

**Source:** `std::fs::Metadata` (obtained via `fs::metadata()`, `fs::symlink_metadata()`, or `File::metadata()`)

**Field Mapping:**

| FileAttr Field | Source |
|---|---|
| `ino` | parameter `ino` |
| `size` | `meta.size()` |
| `blocks` | `meta.blocks()` |
| `atime` | `UNIX_EPOCH + Duration::from_secs(meta.atime() as u64)` |
| `mtime` | `UNIX_EPOCH + Duration::from_secs(meta.mtime() as u64)` |
| `ctime` | `UNIX_EPOCH + Duration::from_secs(meta.ctime() as u64)` |
| `crtime` | same as `ctime` (no birthtime on Linux) |
| `kind` | `meta.is_dir()` → Directory, `meta.is_symlink()` → Symlink, else RegularFile |
| `perm` | `meta.mode() & 0o7777` as u16 |
| `nlink` | `meta.nlink()` as u32 |
| `uid` | `meta.uid()` |
| `gid` | `meta.gid()` |
| `rdev` | `meta.rdev()` as u32 |
| `blksize` | `meta.blksize()` as u32 |
| `flags` | 0 |

---

### attr_from_nix_stat(ino: u64, meta: &libc::stat) -> FileAttr

Build `FileAttr` from a nix stat result. Used for temp files where we have an fd but not a `std::fs::Metadata`.

**Source:** `libc::stat` (obtained via `nix::sys::stat::fstat(fd)`)

**Field Mapping:**

| FileAttr Field | Source |
|---|---|
| `ino` | parameter `ino` |
| `size` | `meta.st_size as u64` |
| `blocks` | `meta.st_blocks as u64` |
| `atime` | `UNIX_EPOCH + Duration::from_secs(meta.st_atime as u64)` |
| `mtime` | `UNIX_EPOCH + Duration::from_secs(meta.st_mtime as u64)` |
| `ctime` | `UNIX_EPOCH + Duration::from_secs(meta.st_ctime as u64)` |
| `crtime` | same as `ctime` |
| `kind` | `S_IFDIR` → Directory, `S_IFLNK` → Symlink, else RegularFile |
| `perm` | `meta.st_mode & 0o7777` as u16 |
| `nlink` | `meta.st_nlink as u32` |
| `uid` | `meta.st_uid` |
| `gid` | `meta.st_gid` |
| `rdev` | `meta.st_rdev as u32` |
| `blksize` | 4096 (hardcoded, no direct mapping) |
| `flags` | 0 |

---

### attr_from_daemon(ino: u64, meta: &FileMetadata, kind: FileType) -> FileAttr

Build `FileAttr` from metadata stored in the syncing daemon.

**Source:** `crate::syncing::proto::FileMetadata`

**Field Mapping:**

| FileAttr Field | Source |
|---|---|
| `ino` | parameter `ino` |
| `size` | `meta.size` |
| `blocks` | `(meta.size + 511) / 512` (calculated) |
| `atime` | `UNIX_EPOCH + Duration::from_secs(meta.atime)` |
| `mtime` | `UNIX_EPOCH + Duration::from_secs(meta.mtime)` |
| `ctime` | `UNIX_EPOCH + Duration::from_secs(meta.ctime)` |
| `crtime` | same as `ctime` |
| `kind` | parameter `kind` |
| `perm` | `meta.mode & 0o7777` as u16 |
| `nlink` | 1 (hardcoded, no multi-link support in daemon) |
| `uid` | `meta.uid` |
| `gid` | `meta.gid` |
| `rdev` | 0 (hardcoded, not stored) |
| `blksize` | 4096 (hardcoded) |
| `flags` | 0 |

## FileMetadata Struct (for reference)

From `syncing/proto.rs`:

```rust
struct FileMetadata {
    size: u64,      // file size in bytes
    mode: u32,      // file mode (permissions + type)
    uid: u32,      // owner uid
    gid: u32,      // owner gid
    mtime: u64,    // modification time (Unix timestamp)
    atime: u64,    // access time (Unix timestamp)
    ctime: u64,    // change time (Unix timestamp)
}
```

Note: The `mode` field contains both permission bits and file type bits (e.g., `S_IFREG`, `S_IFLNK`, `S_IFDIR`). Extract permissions via `mode & 0o7777`.

## Usage in fs.rs

These functions are used by:

| Function | Uses |
|---|---|
| `lookup` | `attr_from_meta` (Passthrough), `attr_from_daemon` (FuseOnly/CoW) |
| `getattr` | `stat_path` → delegates to above |
| `setattr` | `attr_from_meta` (Passthrough), `attr_from_nix_stat` (temp files), `attr_from_daemon` (daemon) |
| `create` | `attr_from_meta` (Passthrough), `attr_from_daemon` (FuseOnly/CoW) |
| `readdir` | N/A (uses FuseEntry kind directly) |
| `symlink` | `attr_from_meta` (Passthrough), `attr_from_daemon` (FuseOnly/CoW) |
| `mkdir` | `attr_from_meta` (Passthrough), `attr_from_daemon` (FuseOnly/CoW) |

## Public API

```rust
pub fn attr_from_meta(ino: u64, meta: &std::fs::Metadata) -> FileAttr
pub fn attr_from_nix_stat(ino: u64, meta: &libc::stat) -> FileAttr
pub fn attr_from_daemon(ino: u64, meta: &FileMetadata, kind: FileType) -> FileAttr
```
