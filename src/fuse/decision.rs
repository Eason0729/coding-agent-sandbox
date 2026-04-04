use std::collections::BTreeMap;

use fuser::FileType;

use crate::error::{Error, Result};
use crate::fuse::state::{
    CreateState, MkdirState, OpenState, ReaddirState, ReadlinkState, RenameState, RmdirState,
    SetattrState, StatState, UnlinkState,
};
use crate::syncing::proto::{EntryType, FuseEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatDecision {
    UseReal,
    UseFuse,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDecision {
    NotFound,
    Error,
    OpenReal,
    OpenObject {
        existing_object_id: Option<u64>,
        needs_ensure: bool,
        copy_up_from_real: bool,
        delete_whiteout: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenTransitionStep {
    RejectWhiteout,
    OpenReal,
    EnsureObject,
    CopyUpFromReal,
    DeleteWhiteout,
    OpenObject,
    ErrorMissingObjectPath,
}

fn is_whiteout_entry(entry: Option<&FuseEntry>) -> bool {
    entry
        .map(|e| e.entry_type == EntryType::Whiteout)
        .unwrap_or(false)
}

fn compute_open_behavior(state: &OpenState) -> (OpenDecision, Vec<OpenTransitionStep>) {
    let mut transitions = Vec::new();

    if is_whiteout_entry(state.fuse_entry.as_ref()) {
        transitions.push(OpenTransitionStep::RejectWhiteout);
        return (OpenDecision::NotFound, transitions);
    }

    let object_id = state.fuse_entry.as_ref().and_then(|e| e.object_id);
    let has_object_path = state.object_path.is_some();

    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => {
            transitions.push(OpenTransitionStep::OpenReal);
            (OpenDecision::OpenReal, transitions)
        }
        crate::fuse::policy::AccessMode::FuseOnly => {
            let needs_ensure = object_id.is_none() || !has_object_path;
            if needs_ensure {
                transitions.push(OpenTransitionStep::EnsureObject);
            }
            transitions.push(OpenTransitionStep::OpenObject);
            (
                OpenDecision::OpenObject {
                    existing_object_id: object_id,
                    needs_ensure,
                    copy_up_from_real: false,
                    delete_whiteout: false,
                },
                transitions,
            )
        }
        crate::fuse::policy::AccessMode::CopyOnWrite => {
            if !state.need_write {
                match (object_id, has_object_path) {
                    (Some(id), true) => {
                        transitions.push(OpenTransitionStep::OpenObject);
                        (
                            OpenDecision::OpenObject {
                                existing_object_id: Some(id),
                                needs_ensure: false,
                                copy_up_from_real: false,
                                delete_whiteout: false,
                            },
                            transitions,
                        )
                    }
                    (Some(_), false) => {
                        transitions.push(OpenTransitionStep::ErrorMissingObjectPath);
                        (OpenDecision::Error, transitions)
                    }
                    (None, _) => {
                        transitions.push(OpenTransitionStep::OpenReal);
                        (OpenDecision::OpenReal, transitions)
                    }
                }
            } else {
                let had_object = object_id.is_some();
                let needs_ensure = object_id.is_none() || !has_object_path;
                if needs_ensure {
                    transitions.push(OpenTransitionStep::EnsureObject);
                }
                let copy_up_from_real =
                    !had_object && !state.truncate_requested && state.real_exists;
                if copy_up_from_real {
                    transitions.push(OpenTransitionStep::CopyUpFromReal);
                }
                transitions.push(OpenTransitionStep::DeleteWhiteout);
                transitions.push(OpenTransitionStep::OpenObject);
                (
                    OpenDecision::OpenObject {
                        existing_object_id: object_id,
                        needs_ensure,
                        copy_up_from_real,
                        delete_whiteout: true,
                    },
                    transitions,
                )
            }
        }
    }
}

pub fn decide_open_with_transitions(state: &OpenState) -> (OpenDecision, Vec<OpenTransitionStep>) {
    compute_open_behavior(state)
}

pub fn decide_open(state: &OpenState) -> OpenDecision {
    decide_open_with_transitions(state).0
}

pub fn extract_open_transitions(state: &OpenState) -> Vec<OpenTransitionStep> {
    decide_open_with_transitions(state).1
}

#[derive(Debug)]
pub enum CreateDecision {
    CreateReal,
    CreateObject,
}

pub fn decide_create(state: &CreateState) -> CreateDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => CreateDecision::CreateReal,
        crate::fuse::policy::AccessMode::FuseOnly
        | crate::fuse::policy::AccessMode::CopyOnWrite => CreateDecision::CreateObject,
    }
}

#[derive(Debug)]
pub enum UnlinkDecision {
    RemoveReal,
    Whiteout,
}

pub fn decide_unlink(state: &UnlinkState) -> UnlinkDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => UnlinkDecision::RemoveReal,
        crate::fuse::policy::AccessMode::FuseOnly
        | crate::fuse::policy::AccessMode::CopyOnWrite => UnlinkDecision::Whiteout,
    }
}

#[derive(Debug)]
pub enum RmdirDecision {
    RemoveReal,
    WhiteoutRecursive,
}

pub fn decide_rmdir(state: &RmdirState) -> RmdirDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => RmdirDecision::RemoveReal,
        crate::fuse::policy::AccessMode::FuseOnly
        | crate::fuse::policy::AccessMode::CopyOnWrite => RmdirDecision::WhiteoutRecursive,
    }
}

#[derive(Debug)]
pub enum RenameDecision {
    RenameReal,
    RenameFuseFileOrSymlink,
    RenameFuseTree,
}

#[derive(Debug)]
pub enum SetattrDecision {
    UpdateOpenHandle,
    UpdateRealFs,
    UpdateDaemonMeta,
}

pub fn decide_setattr(state: &SetattrState) -> SetattrDecision {
    if state.fh_present && state.has_open_handle {
        SetattrDecision::UpdateOpenHandle
    } else {
        match state.access_mode {
            crate::fuse::policy::AccessMode::Passthrough => SetattrDecision::UpdateRealFs,
            crate::fuse::policy::AccessMode::FuseOnly
            | crate::fuse::policy::AccessMode::CopyOnWrite => SetattrDecision::UpdateDaemonMeta,
        }
    }
}

#[derive(Debug)]
pub enum ReadlinkDecision {
    UseFuse,
    UseReal,
    NotFound,
}

pub fn decide_readlink(state: &ReadlinkState) -> ReadlinkDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => ReadlinkDecision::UseReal,
        crate::fuse::policy::AccessMode::FuseOnly => match &state.fuse_entry {
            Some(entry) if entry.entry_type == EntryType::Symlink => ReadlinkDecision::UseFuse,
            Some(entry) if entry.entry_type == EntryType::Whiteout => ReadlinkDecision::NotFound,
            _ => ReadlinkDecision::NotFound,
        },
        crate::fuse::policy::AccessMode::CopyOnWrite => match &state.fuse_entry {
            Some(entry) if entry.entry_type == EntryType::Whiteout => ReadlinkDecision::NotFound,
            Some(entry) if entry.entry_type == EntryType::Symlink => ReadlinkDecision::UseFuse,
            _ => ReadlinkDecision::UseReal,
        },
    }
}

#[derive(Debug)]
pub enum MkdirDecision {
    CreateReal,
    CreateDaemon,
}

pub fn decide_mkdir(state: &MkdirState) -> MkdirDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => MkdirDecision::CreateReal,
        crate::fuse::policy::AccessMode::FuseOnly
        | crate::fuse::policy::AccessMode::CopyOnWrite => MkdirDecision::CreateDaemon,
    }
}

pub fn decide_rename(state: &RenameState) -> RenameDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => RenameDecision::RenameReal,
        crate::fuse::policy::AccessMode::FuseOnly
        | crate::fuse::policy::AccessMode::CopyOnWrite => {
            let is_dir = state
                .from_entry
                .as_ref()
                .map(|e| e.entry_type == EntryType::Dir)
                .unwrap_or(false);
            if is_dir {
                RenameDecision::RenameFuseTree
            } else {
                RenameDecision::RenameFuseFileOrSymlink
            }
        }
    }
}

pub fn decide_stat(state: &StatState) -> StatDecision {
    match state.access_mode {
        crate::fuse::policy::AccessMode::Passthrough => {
            if state.real_exists {
                StatDecision::UseReal
            } else {
                StatDecision::NotFound
            }
        }
        crate::fuse::policy::AccessMode::FuseOnly => match &state.fuse_entry {
            Some(entry) if entry.entry_type != EntryType::Whiteout => StatDecision::UseFuse,
            _ => StatDecision::NotFound,
        },
        crate::fuse::policy::AccessMode::CopyOnWrite => match &state.fuse_entry {
            Some(entry) if entry.entry_type == EntryType::Whiteout => StatDecision::NotFound,
            Some(_) => StatDecision::UseFuse,
            None => {
                if state.real_exists {
                    StatDecision::UseReal
                } else {
                    StatDecision::NotFound
                }
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildDecision {
    Hide,
    ShowReal,
    ShowFuse,
    DontCareCollision,
}

#[derive(Debug)]
pub struct ReaddirDecision {
    pub per_child: BTreeMap<Vec<u8>, ChildDecision>,
}

fn decide_child(
    mode: crate::fuse::policy::AccessMode,
    has_real: bool,
    has_fuse: bool,
    fuse_is_whiteout: bool,
) -> ChildDecision {
    if fuse_is_whiteout {
        return ChildDecision::Hide;
    }

    match mode {
        crate::fuse::policy::AccessMode::Passthrough => match (has_real, has_fuse) {
            (false, false) => ChildDecision::Hide,
            (true, false) => ChildDecision::ShowReal,
            (false, true) => ChildDecision::ShowFuse,
            (true, true) => ChildDecision::DontCareCollision,
        },
        crate::fuse::policy::AccessMode::FuseOnly => match (has_real, has_fuse) {
            (_, true) => ChildDecision::ShowFuse,
            _ => ChildDecision::Hide,
        },
        crate::fuse::policy::AccessMode::CopyOnWrite => match (has_real, has_fuse) {
            (_, true) => ChildDecision::ShowFuse,
            (true, false) => ChildDecision::ShowReal,
            _ => ChildDecision::Hide,
        },
    }
}

pub fn decide_readdir(state: &ReaddirState) -> ReaddirDecision {
    let mut per_child = BTreeMap::new();
    for (name, child) in &state.children {
        let has_real = child.real.is_some();
        let (has_fuse, fuse_is_whiteout) = match &child.fuse {
            Some(f) => (true, f.entry_type == EntryType::Whiteout),
            None => (false, false),
        };
        per_child.insert(
            name.clone(),
            decide_child(
                state.access_mode.clone(),
                has_real,
                has_fuse,
                fuse_is_whiteout,
            ),
        );
    }
    ReaddirDecision { per_child }
}

pub fn validate_readdir_decision(state: &ReaddirState, decision: &ReaddirDecision) -> Result<()> {
    for (name, child_decision) in &decision.per_child {
        let Some(state_child) = state.children.get(name) else {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "decision contains unknown child",
            )));
        };
        match child_decision {
            ChildDecision::ShowReal if state_child.real.is_none() => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "ShowReal without real child",
                )));
            }
            ChildDecision::ShowFuse if state_child.fuse.is_none() => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "ShowFuse without fuse child",
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn choose_visible_child(
    state: &ReaddirState,
    name: &[u8],
    child_decision: ChildDecision,
) -> Option<(FileType, std::path::PathBuf)> {
    let child = state.children.get(name)?;
    match child_decision {
        ChildDecision::Hide => None,
        ChildDecision::ShowReal => child.real.as_ref().map(|r| (r.kind, r.path.clone())),
        ChildDecision::ShowFuse => child.fuse.as_ref().and_then(|f| {
            let kind = match f.entry_type {
                EntryType::Dir => Some(FileType::Directory),
                EntryType::Symlink => Some(FileType::Symlink),
                EntryType::File => Some(FileType::RegularFile),
                EntryType::Whiteout => None,
            }?;
            Some((kind, f.path.clone()))
        }),
        ChildDecision::DontCareCollision => {
            if let Some(real) = &child.real {
                Some((real.kind, real.path.clone()))
            } else {
                child.fuse.as_ref().and_then(|f| {
                    let kind = match f.entry_type {
                        EntryType::Dir => Some(FileType::Directory),
                        EntryType::Symlink => Some(FileType::Symlink),
                        EntryType::File => Some(FileType::RegularFile),
                        EntryType::Whiteout => None,
                    }?;
                    Some((kind, f.path.clone()))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use fuser::FileType;

    use super::{
        decide_mkdir, decide_open, decide_open_with_transitions, decide_readdir, decide_readlink,
        decide_rename, decide_setattr, decide_stat, extract_open_transitions, ChildDecision,
        MkdirDecision, OpenDecision, OpenTransitionStep, ReadlinkDecision, RenameDecision,
        SetattrDecision, StatDecision,
    };
    use crate::fuse::policy::AccessMode;
    use crate::fuse::state::{
        FuseChild, MkdirState, OpenState, ReaddirChildState, ReaddirState, ReadlinkState,
        RealChild, RenameState, SetattrState,
    };
    use crate::syncing::proto::{EntryType, FileMetadata, FuseEntry};

    fn dummy_meta() -> FileMetadata {
        FileMetadata {
            size: 0,
            mode: libc::S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            atime: 0,
            ctime: 0,
        }
    }

    fn fuse_entry(entry_type: EntryType, object_id: Option<u64>) -> FuseEntry {
        FuseEntry {
            entry_type,
            metadata: dummy_meta(),
            object_id,
            symlink_target: None,
        }
    }

    fn open_state(
        access_mode: AccessMode,
        need_write: bool,
        truncate_requested: bool,
        real_exists: bool,
        fuse_entry: Option<FuseEntry>,
        object_path: Option<PathBuf>,
    ) -> OpenState {
        OpenState {
            access_mode,
            path: PathBuf::from("/x"),
            need_write,
            truncate_requested,
            real_exists,
            fuse_entry,
            object_path,
        }
    }

    fn assert_transition_sequence_valid(
        decision: &OpenDecision,
        transitions: &[OpenTransitionStep],
    ) {
        assert!(!transitions.is_empty(), "transitions must not be empty");

        let mut last_rank = 0u8;
        for step in transitions {
            let rank = match step {
                OpenTransitionStep::RejectWhiteout => 10,
                OpenTransitionStep::OpenReal => 20,
                OpenTransitionStep::ErrorMissingObjectPath => 20,
                OpenTransitionStep::EnsureObject => 30,
                OpenTransitionStep::CopyUpFromReal => 40,
                OpenTransitionStep::DeleteWhiteout => 50,
                OpenTransitionStep::OpenObject => 60,
            };
            assert!(
                rank >= last_rank,
                "transitions are out-of-order: {transitions:?}"
            );
            last_rank = rank;
        }

        match decision {
            OpenDecision::NotFound => {
                assert_eq!(
                    transitions,
                    [OpenTransitionStep::RejectWhiteout],
                    "not-found must be whiteout rejection"
                );
            }
            OpenDecision::Error => {
                assert_eq!(
                    transitions,
                    [OpenTransitionStep::ErrorMissingObjectPath],
                    "error must be missing object-path"
                );
            }
            OpenDecision::OpenReal => {
                assert_eq!(
                    transitions,
                    [OpenTransitionStep::OpenReal],
                    "open-real must be a single-step plan"
                );
            }
            OpenDecision::OpenObject {
                needs_ensure,
                copy_up_from_real,
                delete_whiteout,
                ..
            } => {
                assert_eq!(
                    transitions.last(),
                    Some(&OpenTransitionStep::OpenObject),
                    "open-object must end with OpenObject"
                );
                assert_eq!(
                    transitions.contains(&OpenTransitionStep::EnsureObject),
                    *needs_ensure,
                    "EnsureObject presence mismatch"
                );
                assert_eq!(
                    transitions.contains(&OpenTransitionStep::CopyUpFromReal),
                    *copy_up_from_real,
                    "CopyUpFromReal presence mismatch"
                );
                assert_eq!(
                    transitions.contains(&OpenTransitionStep::DeleteWhiteout),
                    *delete_whiteout,
                    "DeleteWhiteout presence mismatch"
                );
                if *copy_up_from_real {
                    assert!(
                        *needs_ensure,
                        "copy-up is only valid on first-write object materialization"
                    );
                }
            }
        }
    }

    #[test]
    fn passthrough_collision_is_dontcare() {
        let mut children = BTreeMap::new();
        children.insert(
            b"x".to_vec(),
            ReaddirChildState {
                real: Some(RealChild {
                    kind: FileType::RegularFile,
                    path: PathBuf::from("/real/x"),
                }),
                fuse: Some(FuseChild {
                    entry_type: EntryType::File,
                    path: PathBuf::from("/fuse/x"),
                }),
            },
        );
        let state = ReaddirState {
            access_mode: AccessMode::Passthrough,
            children,
        };

        let decision = decide_readdir(&state);
        assert_eq!(
            decision.per_child.get(b"x" as &[u8]),
            Some(&ChildDecision::DontCareCollision)
        );
    }

    #[test]
    fn whiteout_hides_for_all_modes() {
        for mode in [
            AccessMode::Passthrough,
            AccessMode::FuseOnly,
            AccessMode::CopyOnWrite,
        ] {
            let mut children = BTreeMap::new();
            children.insert(
                b"x".to_vec(),
                ReaddirChildState {
                    real: Some(RealChild {
                        kind: FileType::RegularFile,
                        path: PathBuf::from("/real/x"),
                    }),
                    fuse: Some(FuseChild {
                        entry_type: EntryType::Whiteout,
                        path: PathBuf::from("/fuse/x"),
                    }),
                },
            );
            let state = ReaddirState {
                access_mode: mode,
                children,
            };
            let decision = decide_readdir(&state);
            assert_eq!(
                decision.per_child.get(b"x" as &[u8]),
                Some(&ChildDecision::Hide)
            );
        }
    }

    #[test]
    fn stat_behavior_matrix_all_modes_including_whiteout() {
        let cases = vec![
            // Passthrough ignores fuse-side entry and only checks real existence.
            (
                AccessMode::Passthrough,
                true,
                Some(fuse_entry(EntryType::Whiteout, None)),
                StatDecision::UseReal,
            ),
            (AccessMode::Passthrough, false, None, StatDecision::NotFound),
            // FuseOnly shows non-whiteout fuse entries only.
            (
                AccessMode::FuseOnly,
                true,
                Some(fuse_entry(EntryType::File, Some(1))),
                StatDecision::UseFuse,
            ),
            (
                AccessMode::FuseOnly,
                true,
                Some(fuse_entry(EntryType::Whiteout, None)),
                StatDecision::NotFound,
            ),
            // CoW prefers fuse entry, whiteout masks real, fallback to real when no fuse entry.
            (
                AccessMode::CopyOnWrite,
                true,
                Some(fuse_entry(EntryType::File, Some(2))),
                StatDecision::UseFuse,
            ),
            (
                AccessMode::CopyOnWrite,
                true,
                Some(fuse_entry(EntryType::Whiteout, None)),
                StatDecision::NotFound,
            ),
            (AccessMode::CopyOnWrite, true, None, StatDecision::UseReal),
            (AccessMode::CopyOnWrite, false, None, StatDecision::NotFound),
        ];

        for (mode, real_exists, fuse, expected) in cases {
            let state = crate::fuse::state::StatState {
                access_mode: mode,
                real_exists,
                fuse_entry: fuse,
            };
            assert_eq!(decide_stat(&state), expected);
        }
    }

    #[test]
    fn readdir_child_resolution_matrix_matches_policy_table() {
        let modes = [
            AccessMode::Passthrough,
            AccessMode::FuseOnly,
            AccessMode::CopyOnWrite,
        ];

        for mode in modes {
            for has_real in [false, true] {
                for fuse_variant in [None, Some(EntryType::File), Some(EntryType::Whiteout)] {
                    let has_fuse = fuse_variant.is_some();
                    let whiteout = matches!(fuse_variant, Some(EntryType::Whiteout));

                    let mut children = BTreeMap::new();
                    children.insert(
                        b"x".to_vec(),
                        ReaddirChildState {
                            real: has_real.then(|| RealChild {
                                kind: FileType::RegularFile,
                                path: PathBuf::from("/real/x"),
                            }),
                            fuse: fuse_variant.map(|entry_type| FuseChild {
                                entry_type,
                                path: PathBuf::from("/fuse/x"),
                            }),
                        },
                    );

                    let state = ReaddirState {
                        access_mode: mode.clone(),
                        children,
                    };
                    let decision = decide_readdir(&state);
                    let got = decision
                        .per_child
                        .get(b"x" as &[u8])
                        .copied()
                        .expect("child decision exists");

                    let expected = if whiteout {
                        ChildDecision::Hide
                    } else {
                        match mode {
                            AccessMode::Passthrough => match (has_real, has_fuse) {
                                (false, false) => ChildDecision::Hide,
                                (true, false) => ChildDecision::ShowReal,
                                (false, true) => ChildDecision::ShowFuse,
                                (true, true) => ChildDecision::DontCareCollision,
                            },
                            AccessMode::FuseOnly => {
                                if has_fuse {
                                    ChildDecision::ShowFuse
                                } else {
                                    ChildDecision::Hide
                                }
                            }
                            AccessMode::CopyOnWrite => {
                                if has_fuse {
                                    ChildDecision::ShowFuse
                                } else if has_real {
                                    ChildDecision::ShowReal
                                } else {
                                    ChildDecision::Hide
                                }
                            }
                        }
                    };

                    assert_eq!(
                        got, expected,
                        "mode={mode:?} has_real={has_real} has_fuse={has_fuse} whiteout={whiteout}"
                    );
                }
            }
        }
    }

    #[test]
    fn open_cow_read_prefers_real_without_object() {
        let state = OpenState {
            access_mode: AccessMode::CopyOnWrite,
            path: PathBuf::from("/x"),
            need_write: false,
            truncate_requested: false,
            real_exists: true,
            fuse_entry: None,
            object_path: None,
        };
        assert!(matches!(decide_open(&state), OpenDecision::OpenReal));
    }

    #[test]
    fn open_cow_first_write_requests_copy_up() {
        let state = OpenState {
            access_mode: AccessMode::CopyOnWrite,
            path: PathBuf::from("/x"),
            need_write: true,
            truncate_requested: false,
            real_exists: true,
            fuse_entry: None,
            object_path: None,
        };
        match decide_open(&state) {
            OpenDecision::OpenObject {
                needs_ensure,
                copy_up_from_real,
                delete_whiteout,
                ..
            } => {
                assert!(needs_ensure);
                assert!(copy_up_from_real);
                assert!(delete_whiteout);
            }
            _ => panic!("unexpected decision"),
        }
    }

    #[test]
    fn open_transition_trace_first_write_copy_up_before_open() {
        let state = open_state(AccessMode::CopyOnWrite, true, false, true, None, None);
        let (decision, transitions) = decide_open_with_transitions(&state);

        assert_eq!(
            transitions,
            vec![
                OpenTransitionStep::EnsureObject,
                OpenTransitionStep::CopyUpFromReal,
                OpenTransitionStep::DeleteWhiteout,
                OpenTransitionStep::OpenObject,
            ]
        );
        assert_transition_sequence_valid(&decision, &transitions);
    }

    #[test]
    fn open_transition_trace_truncate_skips_copy_up() {
        let state = open_state(AccessMode::CopyOnWrite, true, true, true, None, None);
        let (decision, transitions) = decide_open_with_transitions(&state);

        assert_eq!(
            transitions,
            vec![
                OpenTransitionStep::EnsureObject,
                OpenTransitionStep::DeleteWhiteout,
                OpenTransitionStep::OpenObject,
            ]
        );
        assert_transition_sequence_valid(&decision, &transitions);
    }

    #[test]
    fn open_transition_trace_whiteout_is_terminal_not_found() {
        let state = open_state(
            AccessMode::CopyOnWrite,
            true,
            false,
            true,
            Some(fuse_entry(EntryType::Whiteout, None)),
            None,
        );
        let (decision, transitions) = decide_open_with_transitions(&state);

        assert_eq!(decision, OpenDecision::NotFound);
        assert_eq!(transitions, vec![OpenTransitionStep::RejectWhiteout]);
        assert_eq!(extract_open_transitions(&state), transitions);
    }

    #[test]
    fn open_decision_transition_model_check_exhaustive() {
        let modes = [
            AccessMode::Passthrough,
            AccessMode::FuseOnly,
            AccessMode::CopyOnWrite,
        ];

        let fuse_variants = vec![
            None,
            Some(fuse_entry(EntryType::Whiteout, None)),
            Some(fuse_entry(EntryType::File, None)),
            Some(fuse_entry(EntryType::File, Some(11))),
            Some(fuse_entry(EntryType::Dir, None)),
            Some(fuse_entry(EntryType::Symlink, None)),
        ];

        for mode in modes {
            for need_write in [false, true] {
                for truncate_requested in [false, true] {
                    for real_exists in [false, true] {
                        for fuse in fuse_variants.clone() {
                            let object_path_variants: &[Option<PathBuf>] =
                                match fuse.as_ref().and_then(|e| e.object_id) {
                                    Some(_) => &[None, Some(PathBuf::from("/objects/11"))],
                                    None => &[None],
                                };

                            for object_path in object_path_variants {
                                let state = open_state(
                                    mode.clone(),
                                    need_write,
                                    truncate_requested,
                                    real_exists,
                                    fuse.clone(),
                                    object_path.clone(),
                                );

                                let (decision, transitions) = decide_open_with_transitions(&state);
                                assert_transition_sequence_valid(&decision, &transitions);

                                if matches!(
                                    state.fuse_entry.as_ref().map(|e| &e.entry_type),
                                    Some(EntryType::Whiteout)
                                ) {
                                    assert_eq!(decision, OpenDecision::NotFound);
                                    continue;
                                }

                                match state.access_mode {
                                    AccessMode::Passthrough => {
                                        assert_eq!(decision, OpenDecision::OpenReal);
                                    }
                                    AccessMode::FuseOnly => {
                                        assert!(matches!(
                                            decision,
                                            OpenDecision::OpenObject { .. }
                                        ));
                                    }
                                    AccessMode::CopyOnWrite if !state.need_write => {
                                        let object_id =
                                            state.fuse_entry.as_ref().and_then(|e| e.object_id);
                                        match (object_id, state.object_path.is_some()) {
                                            (Some(_), true) => {
                                                assert!(matches!(
                                                    decision,
                                                    OpenDecision::OpenObject {
                                                        needs_ensure: false,
                                                        copy_up_from_real: false,
                                                        delete_whiteout: false,
                                                        ..
                                                    }
                                                ));
                                            }
                                            (Some(_), false) => {
                                                assert_eq!(decision, OpenDecision::Error);
                                            }
                                            (None, _) => {
                                                assert_eq!(decision, OpenDecision::OpenReal)
                                            }
                                        }
                                    }
                                    AccessMode::CopyOnWrite => {
                                        assert!(matches!(
                                            decision,
                                            OpenDecision::OpenObject { .. }
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rename_uses_tree_for_dir_entry() {
        let state = RenameState {
            access_mode: AccessMode::CopyOnWrite,
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
            from_entry: Some(FuseEntry {
                entry_type: EntryType::Dir,
                metadata: FileMetadata {
                    size: 0,
                    mode: libc::S_IFDIR,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                    atime: 0,
                    ctime: 0,
                },
                object_id: None,
                symlink_target: None,
            }),
        };
        assert!(matches!(
            decide_rename(&state),
            RenameDecision::RenameFuseTree
        ));
    }

    #[test]
    fn setattr_open_handle_wins() {
        let state = SetattrState {
            access_mode: AccessMode::Passthrough,
            path: PathBuf::from("/x"),
            fh_present: true,
            has_open_handle: true,
            mode: Some(0o644),
            uid: None,
            gid: None,
            size: None,
        };
        assert!(matches!(
            decide_setattr(&state),
            SetattrDecision::UpdateOpenHandle
        ));
    }

    #[test]
    fn readlink_whiteout_is_notfound() {
        let state = ReadlinkState {
            access_mode: AccessMode::CopyOnWrite,
            path: PathBuf::from("/x"),
            fuse_entry: Some(FuseEntry {
                entry_type: EntryType::Whiteout,
                metadata: FileMetadata {
                    size: 0,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                    atime: 0,
                    ctime: 0,
                },
                object_id: None,
                symlink_target: None,
            }),
        };
        assert!(matches!(
            decide_readlink(&state),
            ReadlinkDecision::NotFound
        ));
    }

    #[test]
    fn mkdir_passthrough_is_real() {
        let state = MkdirState {
            access_mode: AccessMode::Passthrough,
            path: PathBuf::from("/x"),
        };
        assert!(matches!(decide_mkdir(&state), MkdirDecision::CreateReal));
    }
}
