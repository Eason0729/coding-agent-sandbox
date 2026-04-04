use std::collections::BTreeMap;
use std::path::PathBuf;

use fuser::FileType;

use crate::fuse::policy::AccessMode;
use crate::syncing::proto::{EntryType, FuseEntry};

#[derive(Debug)]
pub struct StatState {
    pub access_mode: AccessMode,
    pub real_exists: bool,
    pub fuse_entry: Option<FuseEntry>,
}

#[derive(Debug, Clone)]
pub struct RealChild {
    pub kind: FileType,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct FuseChild {
    pub entry_type: EntryType,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ReaddirChildState {
    pub real: Option<RealChild>,
    pub fuse: Option<FuseChild>,
}

#[derive(Debug, Clone)]
pub struct ReaddirState {
    pub access_mode: AccessMode,
    pub children: BTreeMap<Vec<u8>, ReaddirChildState>,
}

#[derive(Debug, Clone)]
pub struct OpenState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
    pub need_write: bool,
    pub truncate_requested: bool,
    pub real_exists: bool,
    pub fuse_entry: Option<FuseEntry>,
    pub object_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CreateState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UnlinkState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RmdirState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RenameState {
    pub access_mode: AccessMode,
    pub from: PathBuf,
    pub to: PathBuf,
    pub from_entry: Option<FuseEntry>,
}

#[derive(Debug, Clone)]
pub struct SetattrState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
    pub fh_present: bool,
    pub has_open_handle: bool,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ReadlinkState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
    pub fuse_entry: Option<FuseEntry>,
}

#[derive(Debug, Clone)]
pub struct MkdirState {
    pub access_mode: AccessMode,
    pub path: PathBuf,
}
