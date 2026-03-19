use nix::errno::Errno;
use nix::unistd::{Gid, Uid};
use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Stage1Error {
    #[error("failed to get current UID")]
    GetUid,
    #[error("failed to get current GID")]
    GetGid,
    #[error("failed to create user namespace: {0}")]
    CreateUserNs(Errno),
    #[error("failed to write UID mapping: {0}")]
    WriteUidMap(#[source] io::Error),
    #[error("failed to write GID mapping: {0}")]
    WriteGidMap(#[source] io::Error),
    #[error("failed to set UID map: {0}")]
    SetUidMap(Errno),
    #[error("failed to set GID map: {0}")]
    SetGidMap(Errno),
}

pub type Result<T> = std::result::Result<T, Stage1Error>;

pub struct UserNs {
    uid: Uid,
    gid: Gid,
}

impl UserNs {
    pub fn new() -> Result<Self> {
        let uid = Uid::current();
        let gid = Gid::current();

        if uid == Uid::from_raw(0) {
            return Err(Stage1Error::GetUid);
        }
        if gid == Gid::from_raw(0) {
            return Err(Stage1Error::GetGid);
        }

        nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWUSER)
            .map_err(Stage1Error::CreateUserNs)?;

        let uid_map = format!("{} {} 1\n", uid, uid);
        let gid_map = format!("{} {} 1\n", gid, gid);

        Self::write_id_map("/proc/self/setgroups", "deny\n").map_err(Stage1Error::WriteGidMap)?;
        Self::write_id_map("/proc/self/uid_map", &uid_map).map_err(Stage1Error::WriteUidMap)?;
        Self::write_id_map("/proc/self/gid_map", &gid_map).map_err(Stage1Error::WriteGidMap)?;

        Ok(Self { uid, gid })
    }

    fn write_id_map(path: &str, content: &str) -> io::Result<()> {
        std::fs::write(path, content)
    }

    pub fn uid(&self) -> Uid {
        self.uid
    }

    pub fn gid(&self) -> Gid {
        self.gid
    }
}

pub fn create_user_ns() -> Result<UserNs> {
    UserNs::new()
}
