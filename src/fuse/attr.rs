use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};

use crate::syncing::proto::FileMetadata;

pub fn attr_from_meta(ino: u64, meta: &std::fs::Metadata) -> FileAttr {
    let kind = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let atime = UNIX_EPOCH + Duration::from_secs(meta.atime() as u64);
    let mtime = UNIX_EPOCH + Duration::from_secs(meta.mtime() as u64);
    let ctime = UNIX_EPOCH + Duration::from_secs(meta.ctime() as u64);
    FileAttr {
        ino: INodeNo(ino),
        size: meta.size(),
        blocks: meta.blocks(),
        atime,
        mtime,
        ctime,
        crtime: ctime,
        kind,
        perm: (meta.mode() & 0o7777) as u16,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    }
}

#[allow(dead_code)]
pub fn attr_from_nix_stat(ino: u64, meta: &libc::stat) -> FileAttr {
    use std::os::unix::fs::MetadataExt;
    let kind = if meta.st_mode & libc::S_IFDIR as u32 != 0 {
        FileType::Directory
    } else if meta.st_mode & libc::S_IFLNK as u32 != 0 {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let atime = UNIX_EPOCH + Duration::from_secs(meta.st_atime as u64);
    let mtime = UNIX_EPOCH + Duration::from_secs(meta.st_mtime as u64);
    let ctime = UNIX_EPOCH + Duration::from_secs(meta.st_ctime as u64);
    FileAttr {
        ino: INodeNo(ino),
        size: meta.st_size as u64,
        blocks: meta.st_blocks as u64,
        atime,
        mtime,
        ctime,
        crtime: ctime,
        kind,
        perm: (meta.st_mode & 0o7777) as u16,
        nlink: meta.st_nlink as u32,
        uid: meta.st_uid,
        gid: meta.st_gid,
        rdev: meta.st_rdev as u32,
        blksize: 4096,
        flags: 0,
    }
}

pub fn attr_from_daemon(ino: u64, meta: &FileMetadata, kind: FileType) -> FileAttr {
    let atime = UNIX_EPOCH + Duration::from_secs(meta.atime);
    let mtime = UNIX_EPOCH + Duration::from_secs(meta.mtime);
    let ctime = UNIX_EPOCH + Duration::from_secs(meta.ctime);
    FileAttr {
        ino: INodeNo(ino),
        size: meta.size,
        blocks: (meta.size + 511) / 512,
        atime,
        mtime,
        ctime,
        crtime: ctime,
        kind,
        perm: (meta.mode & 0o7777) as u16,
        nlink: 1,
        uid: meta.uid,
        gid: meta.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
