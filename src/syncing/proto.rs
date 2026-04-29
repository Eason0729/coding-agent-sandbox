use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub atime: u64,
    pub ctime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    Whiteout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuseEntry {
    pub entry_type: EntryType,
    pub metadata: FileMetadata,
    pub object_id: Option<u64>,
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    EnsureFileObject {
        path: PathBuf,
        meta: FileMetadata,
    },
    EnsureFileObjectFromReal {
        path: PathBuf,
        meta: FileMetadata,
    },
    GetObjectPath {
        id: u64,
    },
    UpsertFileEntry {
        path: PathBuf,
        object_id: u64,
        meta: FileMetadata,
    },
    PutFileMeta {
        path: PathBuf,
        meta: FileMetadata,
    },
    GetFileMeta {
        path: PathBuf,
    },
    GetEntry {
        path: PathBuf,
    },
    DeleteFile {
        path: PathBuf,
    },
    RenameFile {
        from: PathBuf,
        to: PathBuf,
    },
    PutDir {
        path: PathBuf,
        meta: FileMetadata,
    },
    PutSymlink {
        path: PathBuf,
        target: Vec<u8>,
        meta: FileMetadata,
    },
    PutWhiteout {
        path: PathBuf,
    },
    DeleteWhiteout {
        path: PathBuf,
    },
    ReadDirAll {
        path: PathBuf,
    },
    ListWhiteoutUnder {
        path: PathBuf,
    },
    RenameTree {
        from: PathBuf,
        to: PathBuf,
    },
    LogAccess {
        path: PathBuf,
        operation: String,
        pid: u32,
    },
    Flush,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    EnsureFileObject { id: u64, path: PathBuf },
    GetObjectPath { path: PathBuf },
    UpsertFileEntry,
    PutFileMeta,
    GetFileMeta(Option<FileMetadata>),
    GetEntry(Option<FuseEntry>),
    DeleteFile,
    RenameFile,
    PutDir,
    PutSymlink,
    PutWhiteout,
    DeleteWhiteout,
    DirEntries(Vec<(PathBuf, FuseEntry)>),
    WhiteoutPaths(Vec<PathBuf>),
    RenameTree,
    LogAccess,
    Flush,
    Ok,
    NotFound,
    Error(String),
}

impl Response {
    pub fn error(msg: impl Into<String>) -> Self {
        Response::Error(msg.into())
    }
}

impl FuseEntry {
    pub fn is_whiteout(&self) -> bool {
        self.entry_type == EntryType::Whiteout
    }
    pub fn is_file(&self) -> bool {
        self.entry_type == EntryType::File
    }
    pub fn is_dir(&self) -> bool {
        self.entry_type == EntryType::Dir
    }
    pub fn is_symlink(&self) -> bool {
        self.entry_type == EntryType::Symlink
    }
}
