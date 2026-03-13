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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirMetadata {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub atime: u64,
    pub ctime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuseEntry {
    pub id: u64,
    pub entry_type: EntryType,
    pub metadata: FileMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    Whiteout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub path: PathBuf,
    pub entry: FuseEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    PutObject {
        data: Vec<u8>,
    },
    GetObject {
        id: u64,
    },
    PutFile {
        path: PathBuf,
        data: Vec<u8>,
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
        meta: DirMetadata,
    },
    PutWhiteout {
        path: PathBuf,
    },
    ReadDirAll {
        path: PathBuf,
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
    PutObject { id: u64 },
    GetObject { data: Vec<u8> },
    PutFile { id: u64 },
    PutFileMeta,
    GetFileMeta(Option<FileMetadata>),
    GetEntry(Option<FuseEntry>),
    DeleteFile,
    RenameFile,
    PutDir,
    PutWhiteout,
    DirEntries(Vec<(PathBuf, FuseEntry)>),
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
