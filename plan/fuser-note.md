Due to difficulty that agent working with latest fuser, we document and Key API differences here.

Please search and read latest documentation whenever possible!

## fuser 0.17 Mount API

In fuser 0.17, `fuser::mount2()` requires a `&fuser::Config` instead of `&[MountOption]`:

```rust
// WRONG (fuser 0.17)
fuser::mount2(fs, mountpoint, options)

// CORRECT: Build a Config and use mount2
let mut config = fuser::Config::default();
for opt in options {
    config.add_opt(opt);
}
fuser::mount2(fs, mountpoint, &config)
```

Or use the higher-level `fuser::mount()` if available (which internally builds a Config).

## Mount Options

Use at minimum:
- `MountOption::FSName("cas".to_string())`

**Do not use `AutoUnmount` or `allow_other`.**

`AutoUnmount` requires `config.acl` to be set to something other than `SessionACL::Owner`, but `MountOption::CUSTOM("allow_other")` only appends a string to `mount_options` — it does **not** update `config.acl`. fuser validates the ACL before allowing `AutoUnmount`, so combining them causes the runtime error:

```
auto_unmount requires acl != Owner, got: Owner
```

Since the FUSE daemon runs inside a **private mount namespace**, the mount is automatically torn down when the namespace is destroyed (i.e. when all processes in the namespace exit). `AutoUnmount` is unnecessary.

`allow_other` is likewise unnecessary inside a private user+mount namespace — no other UIDs can see the mount from outside.

## fuser 0.17 API Notes

This implementation uses `fuser` crate version 0.17.0. Key API differences from older versions:

### Newtype Wrappers

Several types are newtype wrappers around primitive types:

```rust
// FileHandle - use .0 to extract inner u64
FileHandle(fh.0)

// OpenFlags - use .0 to extract inner i32  
flags: OpenFlags.0

// Generation - wrap with fuser::Generation()
reply.entry(&ttl, &attr, fuser::Generation(0));
reply.created(&ttl, &attr, fuser::Generation(0), FileHandle(fh), FopenFlags::empty());
```

### Error Handling

`reply.error()` expects `fuser::Errno`, not `i32`:

```rust
use fuser::Errno;

// Use constants
reply.error(Errno::ENOENT);
reply.error(Errno::EACCES);
reply.error(Errno::EPERM);

// Or from i32
reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
```

### ReplyDirectory::add()

Takes 4 arguments (no flags parameter):

```rust
// CORRECT (fuser 0.17)
reply.add(attr.ino, offset as u64, attr.kind, &name)

// WRONG (older versions had 5 args with flags)
reply.add(attr.ino, offset, attr.kind, flags, &name)
```

### NamedTempFile

`NamedTempFile` in fuser 0.17 doesn't have `try_clone()` or `read_to_vec()`. Use `File` methods:

```rust
// Read from NamedTempFile
use std::os::unix::fs::AsRawFd;
use std::os::fd::FromRawFd;
let mut file = unsafe { File::from_raw_fd(tmp.as_raw_fd()) };
let mut buf = Vec::new();
file.read_to_end(&mut buf)?;
```

### Statvfs

`nix::sys::statvfs::Statvfs` doesn't have `fragments()`. Use:

```rust
reply.statfs(
    stat.blocks() as u64,
    stat.blocks_available() as u64,
    stat.files() as u64,
    stat.files_available() as u64,
    stat.block_size() as u64,  // Note: u64, not u32
    4096,
    stat.fragment_size() as u32,  // Use fragment_size, not fragments
    stat.fragment_size() as u32,
);
```
