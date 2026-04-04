use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use fuser::FileType;

use crate::error::{Error, Result};
use crate::fuse::policy::Policy;
use crate::fuse::state::{
    CreateState, FuseChild, MkdirState, OpenState, ReaddirChildState, ReaddirState, ReadlinkState,
    RealChild, RenameState, RmdirState, SetattrState, StatState, UnlinkState,
};
use crate::syncing::proto::EntryType;
use crate::syncing::PooledSyncClient;

pub fn load_stat_state(
    policy: &dyn Policy,
    path: &Path,
    client: &mut PooledSyncClient,
) -> Result<StatState> {
    let access_mode = policy.classify(path);
    let real_exists = std::fs::symlink_metadata(path).is_ok();
    let fuse_entry = client.get_entry(path.to_path_buf()).map_err(Error::from)?;
    Ok(StatState {
        access_mode,
        real_exists,
        fuse_entry,
    })
}

pub fn load_readdir_state(
    policy: &dyn Policy,
    path: &Path,
    client: &mut PooledSyncClient,
) -> Result<ReaddirState> {
    let access_mode = policy.classify(path);
    let mut children: BTreeMap<Vec<u8>, ReaddirChildState> = BTreeMap::new();

    if let Ok(rd) = std::fs::read_dir(path) {
        for ent in rd.flatten() {
            let name = ent.file_name().as_bytes().to_vec();
            let p = ent.path();
            let kind = match ent.file_type() {
                Ok(t) if t.is_dir() => FileType::Directory,
                Ok(t) if t.is_symlink() => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            let entry = children.entry(name).or_default();
            entry.real = Some(RealChild { kind, path: p });
        }
    }

    let fuse_entries = client
        .read_dir_all(path.to_path_buf())
        .map_err(Error::from)?;
    for (child_path, entry) in fuse_entries {
        let Some(name) = child_path.file_name() else {
            continue;
        };
        let key = name.as_bytes().to_vec();
        let slot = children.entry(key).or_default();
        slot.fuse = Some(FuseChild {
            entry_type: entry.entry_type,
            path: child_path,
        });
    }

    Ok(ReaddirState {
        access_mode,
        children,
    })
}

pub fn load_open_state(
    policy: &dyn Policy,
    path: &Path,
    need_write: bool,
    truncate_requested: bool,
    client: &mut PooledSyncClient,
) -> Result<OpenState> {
    let access_mode = policy.classify(path);
    let fuse_entry = client.get_entry(path.to_path_buf()).map_err(Error::from)?;
    let object_path = match fuse_entry.as_ref().and_then(|e| e.object_id) {
        Some(id) => client.get_object_path(id).ok(),
        None => None,
    };
    Ok(OpenState {
        access_mode,
        path: path.to_path_buf(),
        need_write,
        truncate_requested,
        real_exists: path.exists(),
        fuse_entry,
        object_path,
    })
}

pub fn load_create_state(policy: &dyn Policy, path: &Path) -> CreateState {
    CreateState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
    }
}

pub fn load_unlink_state(policy: &dyn Policy, path: &Path) -> Result<UnlinkState> {
    let _ = std::fs::symlink_metadata(path).ok();
    Ok(UnlinkState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
    })
}

pub fn load_rmdir_state(policy: &dyn Policy, path: &Path) -> Result<RmdirState> {
    let _ = std::fs::symlink_metadata(path).ok();
    Ok(RmdirState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
    })
}

pub fn load_setattr_state(
    policy: &dyn Policy,
    path: &Path,
    fh_present: bool,
    has_open_handle: bool,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
) -> SetattrState {
    SetattrState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
        fh_present,
        has_open_handle,
        mode,
        uid,
        gid,
        size,
    }
}

pub fn load_readlink_state(
    policy: &dyn Policy,
    path: &Path,
    client: &mut PooledSyncClient,
) -> Result<ReadlinkState> {
    let fuse_entry = client.get_entry(path.to_path_buf()).map_err(Error::from)?;
    Ok(ReadlinkState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
        fuse_entry,
    })
}

pub fn load_mkdir_state(policy: &dyn Policy, path: &Path) -> MkdirState {
    MkdirState {
        access_mode: policy.classify(path),
        path: path.to_path_buf(),
    }
}

pub fn load_rename_state(
    policy: &dyn Policy,
    from: &Path,
    to: &Path,
    client: &mut PooledSyncClient,
) -> Result<RenameState> {
    let from_entry = client.get_entry(from.to_path_buf()).map_err(Error::from)?;
    let _ = client.get_entry(to.to_path_buf()).map_err(Error::from)?;
    Ok(RenameState {
        access_mode: policy.classify(from),
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        from_entry,
    })
}

pub fn fuse_entry_is_whiteout(entry: Option<&crate::syncing::proto::FuseEntry>) -> bool {
    entry
        .map(|e| e.entry_type == EntryType::Whiteout)
        .unwrap_or(false)
}
