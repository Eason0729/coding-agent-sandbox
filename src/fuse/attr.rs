use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};

use crate::syncing::proto::FileMetadata;

fn system_time_from_unix_i64(secs: i64) -> SystemTime {
    if secs >= 0 {
        return UNIX_EPOCH
            .checked_add(Duration::from_secs(secs as u64))
            .unwrap_or(UNIX_EPOCH);
    }

    UNIX_EPOCH
        .checked_sub(Duration::from_secs(secs.unsigned_abs()))
        .unwrap_or(UNIX_EPOCH)
}

fn system_time_from_unix_u64(secs: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH)
}

pub fn attr_from_meta(ino: u64, meta: &std::fs::Metadata) -> FileAttr {
    let kind = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let atime = system_time_from_unix_i64(meta.atime());
    let mtime = system_time_from_unix_i64(meta.mtime());
    let ctime = system_time_from_unix_i64(meta.ctime());

    fn map_nobody(uid: u32) -> u32 {
        if uid >= 65534 {
            0
        } else {
            uid
        }
    }

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
        uid: map_nobody(meta.uid()),
        gid: map_nobody(meta.gid()),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    }
}

pub fn attr_from_nix_stat(ino: u64, meta: &libc::stat) -> FileAttr {
    let kind = if meta.st_mode & libc::S_IFDIR as u32 != 0 {
        FileType::Directory
    } else if meta.st_mode & libc::S_IFLNK as u32 != 0 {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let atime = system_time_from_unix_i64(meta.st_atime);
    let mtime = system_time_from_unix_i64(meta.st_mtime);
    let ctime = system_time_from_unix_i64(meta.st_ctime);
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
    let atime = system_time_from_unix_u64(meta.atime);
    let mtime = system_time_from_unix_u64(meta.mtime);
    let ctime = system_time_from_unix_u64(meta.ctime);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_i64_negative_is_handled() {
        let t = system_time_from_unix_i64(-1);
        assert!(UNIX_EPOCH.duration_since(t).is_ok());
    }

    #[test]
    fn unix_u64_huge_does_not_panic() {
        let t = system_time_from_unix_u64(u64::MAX);
        assert_eq!(t, UNIX_EPOCH);
    }
}
