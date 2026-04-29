use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use log;
use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio::time::{sleep, Duration};

use crate::syncing::closure::PathTree;
use crate::syncing::disk::{self, FuseMap, SandboxMeta};
use crate::syncing::object::ObjectStore;
use crate::syncing::proto::{EntryType, FuseEntry, Request, Response};

const READY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ServerState {
    pub objects: Arc<ObjectStore>,
    pub fuse_map: DashMap<PathBuf, crate::syncing::proto::FuseEntry>,
    pub path_tree: PathTree,
    pub sandbox_dir: PathBuf,
    pub shm_name: String,
    pub abi_version: u32,
}

/// Shutdown poll contract owned by the caller.
pub trait PollLock {
    fn poll_shutdown<F>(&mut self, on_shutdown: F) -> bool
    where
        F: FnOnce();
}

pub fn fork_and_run<P, F>(sandbox_dir: PathBuf, mut poll_lock: P, on_ready: F) -> nix::Result<()>
where
    P: PollLock,
    F: FnOnce(),
{
    let (mut parent_sock, mut child_sock) = StdUnixStream::pair().map_err(|_| Errno::EIO)?;
    parent_sock.set_nonblocking(true).map_err(|_| Errno::EIO)?;

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            drop(child_sock);

            let deadline = std::time::Instant::now() + READY_TIMEOUT;
            let mut ready = [0u8; 1];
            log::debug!("sync.start event=parent_wait_ready");

            loop {
                match <StdUnixStream as Read>::read(&mut parent_sock, &mut ready) {
                    Ok(1) => {
                        log::debug!("sync.start event=ready_signal_received");
                        on_ready();
                        return Ok(());
                    }
                    Ok(0) => return Err(Errno::EPIPE),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return Err(Errno::EIO),
                    _ => {}
                }

                match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {}
                    Ok(_) => return Err(Errno::ECHILD),
                    Err(e) => return Err(e),
                }

                if std::time::Instant::now() > deadline {
                    return Err(Errno::ETIMEDOUT);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        ForkResult::Child => {
            drop(parent_sock);
            run(
                sandbox_dir,
                move || {
                    let _ = <StdUnixStream as Write>::write_all(&mut child_sock, &[1]);
                    let _ = <StdUnixStream as Write>::flush(&mut child_sock);
                },
                &mut poll_lock,
            );
        }
    }

    Ok(())
}

fn snapshot_state(state: &ServerState) -> (SandboxMeta, FuseMap) {
    let meta = SandboxMeta {
        shm_name: state.shm_name.clone(),
        abi_version: state.abi_version,
        next_id: state.objects.next_id(),
    };
    let fuse_map = FuseMap {
        entries: state
            .fuse_map
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect::<HashMap<_, _>>(),
    };
    (meta, fuse_map)
}

fn upsert_tree(state: &ServerState, path: &Path) {
    state.path_tree.insert(path);
}

fn delete_tree(state: &ServerState, path: &Path) {
    let _ = state.path_tree.remove(path);
}

fn rename_tree(state: &ServerState, from: &Path, to: &Path) {
    let subtree = state.path_tree.subtree_paths(from);
    let _ = state.path_tree.remove(from);

    for old_path in subtree {
        let rel = old_path.strip_prefix(from).unwrap_or(Path::new(""));
        let mut new_path = to.to_path_buf();
        if !rel.as_os_str().is_empty() {
            new_path.push(rel);
        }
        state.path_tree.insert(&new_path);
    }
}

fn build_server_state(sandbox_dir: &Path) -> std::result::Result<Arc<ServerState>, String> {
    let (meta, fuse_map, path_tree) =
        disk::load(&sandbox_dir.to_path_buf()).map_err(|e| e.to_string())?;

    let objects_dir = sandbox_dir.join(".sandbox").join("data").join("objects");
    let object_store = ObjectStore::new(objects_dir, meta.next_id);

    Ok(Arc::new(ServerState {
        objects: Arc::new(object_store),
        fuse_map: DashMap::from_iter(fuse_map.entries),
        path_tree,
        sandbox_dir: sandbox_dir.to_path_buf(),
        shm_name: meta.shm_name,
        abi_version: meta.abi_version,
    }))
}

fn bind_listener(sock_path: &Path) -> std::io::Result<StdUnixListener> {
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(sock_path);

    let listener = StdUnixListener::bind(sock_path)?;
    std::fs::set_permissions(
        sock_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn bind_listener_async(sock_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(sock_path);

    let listener = UnixListener::bind(sock_path)?;
    std::fs::set_permissions(
        sock_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    Ok(listener)
}

fn flush_and_remove_socket(state: &ServerState, sock_path: &Path) {
    let (meta, fuse_map) = snapshot_state(state);
    if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map, &state.path_tree) {
        log::error!("sync.lifecycle event=flush_failed error={e}");
    }
    let _ = std::fs::remove_file(sock_path);
}

pub fn run<P: PollLock>(sandbox_dir: PathBuf, ready: impl FnOnce(), poll_lock: &mut P) {
    let rt = Runtime::new().expect("failed to build tokio runtime");

    rt.block_on(async {
        run_async(sandbox_dir, ready, poll_lock).await;
    });
}

async fn run_async<P: PollLock>(sandbox_dir: PathBuf, ready: impl FnOnce(), poll_lock: &mut P) {
    let state = match build_server_state(&sandbox_dir) {
        Ok(v) => v,
        Err(e) => {
            log::error!("sync.start event=state_init_failed error={e}");
            std::process::exit(1);
        }
    };

    let sock_path = sandbox_dir.join(".sandbox").join("daemon.sock");
    let listener = match bind_listener_async(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!(
                "sync.start event=bind_failed socket={} error={e}",
                sock_path.display()
            );
            std::process::exit(1);
        }
    };

    ready();
    log::info!("sync.start event=ready socket={}", sock_path.display());

    let shutdown_flag = Arc::new(AtomicBool::new(false));

    let accept_flag = Arc::clone(&shutdown_flag);
    let state_ref = Arc::clone(&state);
    let _sock_path_ref = sock_path.clone();

    tokio::spawn(async move {
        while !accept_flag.load(Ordering::Acquire) {
            sleep(Duration::from_millis(100)).await;
        }
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let state_ref = Arc::clone(&state_ref);
                        let shutdown_flag_ref = Arc::clone(&shutdown_flag);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(&state_ref, stream).await {
                                log::error!("sync.conn event=serve_failed error={e}");
                            }
                            if shutdown_flag_ref.load(Ordering::Acquire) {
                                log::debug!("sync.conn event=connection_shutdown");
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("sync.accept event=error error={e}");
                    }
                }
            }
        }

        if poll_lock.poll_shutdown(|| flush_and_remove_socket(state.as_ref(), &sock_path)) {
            shutdown_flag.store(true, Ordering::Release);
            break;
        }
    }

    shutdown_flag.store(true, Ordering::Release);
    sleep(Duration::from_millis(100)).await;

    flush_and_remove_socket(state.as_ref(), &sock_path);

    std::process::exit(0);
}

async fn handle_connection(
    state: &ServerState,
    mut stream: UnixStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let mut length_buf = [0u8; 4];
        match AsyncReadExt::read_exact(&mut stream, &mut length_buf).await {
            Ok(4) => {}
            Ok(_) => return Err("read_exact returned unexpected byte count".into()),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let length = u32::from_le_bytes(length_buf) as usize;

        let mut request_buf = vec![0u8; length];
        AsyncReadExt::read_exact(&mut stream, &mut request_buf).await?;

        let request: Request = postcard::from_bytes(&request_buf)?;
        let response = dispatch(state, request).await;

        let response_data = postcard::to_allocvec(&response)?;
        let response_len = (response_data.len() as u32).to_le_bytes();
        AsyncWriteExt::write_all(&mut stream, &response_len).await?;
        AsyncWriteExt::write_all(&mut stream, &response_data).await?;
        AsyncWriteExt::flush(&mut stream).await?;
    }

    Ok(())
}

async fn dispatch(state: &ServerState, request: Request) -> Response {
    match request {
        Request::EnsureFileObject { path, meta } => {
            let existing = state.fuse_map.get(&path).map(|v| v.clone());
            match existing {
                Some(entry)
                    if entry.entry_type == EntryType::File
                        && entry.object_id.is_some()
                        && entry.symlink_target.is_none() =>
                {
                    let id = entry.object_id.unwrap_or_default();
                    let object_path = state.objects.path_for(id);
                    let new_entry = FuseEntry {
                        entry_type: EntryType::File,
                        metadata: meta,
                        object_id: Some(id),
                        symlink_target: None,
                    };
                    state.fuse_map.insert(path.clone(), new_entry);
                    upsert_tree(state, &path);
                    Response::EnsureFileObject {
                        id,
                        path: object_path,
                    }
                }
                _ => match state.objects.alloc_empty() {
                    Ok(id) => {
                        let object_path = state.objects.path_for(id);
                        let entry = FuseEntry {
                            entry_type: EntryType::File,
                            metadata: meta,
                            object_id: Some(id),
                            symlink_target: None,
                        };
                        state.fuse_map.insert(path.clone(), entry);
                        upsert_tree(state, &path);
                        Response::EnsureFileObject {
                            id,
                            path: object_path,
                        }
                    }
                    Err(e) => Response::error(e.to_string()),
                },
            }
        }
        Request::EnsureFileObjectFromReal { path, meta } => {
            let existing = state.fuse_map.get(&path).map(|v| v.clone());
            match existing {
                Some(entry)
                    if entry.entry_type == EntryType::File
                        && entry.object_id.is_some()
                        && entry.symlink_target.is_none() =>
                {
                    let id = entry.object_id.unwrap_or_default();
                    let object_path = state.objects.path_for(id);
                    let new_entry = FuseEntry {
                        entry_type: EntryType::File,
                        metadata: meta,
                        object_id: Some(id),
                        symlink_target: None,
                    };
                    state.fuse_map.insert(path.clone(), new_entry);
                    upsert_tree(state, &path);
                    Response::EnsureFileObject {
                        id,
                        path: object_path,
                    }
                }
                _ => match state.objects.put_copy_from(&path) {
                    Ok(id) => {
                        let object_path = state.objects.path_for(id);
                        let entry = FuseEntry {
                            entry_type: EntryType::File,
                            metadata: meta,
                            object_id: Some(id),
                            symlink_target: None,
                        };
                        state.fuse_map.insert(path.clone(), entry);
                        upsert_tree(state, &path);
                        Response::EnsureFileObject {
                            id,
                            path: object_path,
                        }
                    }
                    Err(e) => Response::error(e.to_string()),
                },
            }
        }
        Request::GetObjectPath { id } => {
            if state.objects.exists(id) {
                Response::GetObjectPath {
                    path: state.objects.path_for(id),
                }
            } else {
                Response::NotFound
            }
        }
        Request::UpsertFileEntry {
            path,
            object_id,
            meta,
        } => {
            let entry = FuseEntry {
                entry_type: EntryType::File,
                metadata: meta,
                object_id: Some(object_id),
                symlink_target: None,
            };
            state.fuse_map.insert(path.clone(), entry);
            upsert_tree(state, &path);
            Response::UpsertFileEntry
        }
        Request::PutFileMeta { path, meta } => match state.fuse_map.get_mut(&path) {
            Some(mut entry) => {
                entry.metadata = meta;
                Response::PutFileMeta
            }
            None => Response::error("File not found"),
        },
        Request::GetFileMeta { path } => {
            let meta = state.fuse_map.get(&path).map(|e| e.metadata.clone());
            Response::GetFileMeta(meta)
        }
        Request::GetEntry { path } => {
            let entry = state.fuse_map.get(&path).map(|v| v.clone());
            Response::GetEntry(entry)
        }
        Request::DeleteFile { path } => {
            state.fuse_map.remove(&path);
            delete_tree(state, &path);
            Response::DeleteFile
        }
        Request::RenameFile { from, to } => {
            if let Some((_, entry)) = state.fuse_map.remove(&from) {
                state.fuse_map.insert(to.clone(), entry);
                rename_tree(state, &from, &to);
                Response::RenameFile
            } else {
                Response::error("Source path not found")
            }
        }
        Request::PutDir { path, meta } => {
            let entry = FuseEntry {
                entry_type: EntryType::Dir,
                metadata: meta,
                object_id: None,
                symlink_target: None,
            };
            state.fuse_map.insert(path.clone(), entry);
            upsert_tree(state, &path);
            Response::PutDir
        }
        Request::PutSymlink { path, target, meta } => {
            let entry = FuseEntry {
                entry_type: EntryType::Symlink,
                metadata: meta,
                object_id: None,
                symlink_target: Some(target),
            };
            state.fuse_map.insert(path.clone(), entry);
            upsert_tree(state, &path);
            Response::PutSymlink
        }
        Request::PutWhiteout { path } => {
            let entry = FuseEntry {
                entry_type: EntryType::Whiteout,
                metadata: crate::syncing::proto::FileMetadata {
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
            };
            state.fuse_map.insert(path.clone(), entry);
            upsert_tree(state, &path);
            Response::PutWhiteout
        }
        Request::DeleteWhiteout { path } => {
            if let Some(existing) = state.fuse_map.get(&path) {
                if existing.entry_type == EntryType::Whiteout {
                    drop(existing);
                    state.fuse_map.remove(&path);
                }
            }
            Response::DeleteWhiteout
        }
        Request::ReadDirAll { path } => {
            let mut entries = Vec::new();
            for child in state.path_tree.children_of(&path) {
                if let Some(entry) = state.fuse_map.get(&child) {
                    entries.push((child, entry.clone()));
                }
            }

            Response::DirEntries(entries)
        }
        Request::ListWhiteoutUnder { path } => {
            let mut whiteouts = Vec::new();
            for descendant in state.path_tree.descendants_of(&path) {
                if let Some(entry) = state.fuse_map.get(&descendant) {
                    if entry.entry_type == EntryType::Whiteout {
                        whiteouts.push(descendant);
                    }
                }
            }
            Response::WhiteoutPaths(whiteouts)
        }
        Request::RenameTree { from, to } => {
            let entries = state.path_tree.subtree_paths(&from);

            for old_path in entries {
                if let Some((_, entry)) = state.fuse_map.remove(&old_path) {
                    let rel = old_path.strip_prefix(&from).unwrap_or(Path::new(""));
                    let mut new_path = to.clone();
                    if !rel.as_os_str().is_empty() {
                        new_path.push(rel);
                    }
                    state.fuse_map.insert(new_path, entry);
                }
            }
            rename_tree(state, &from, &to);
            Response::RenameTree
        }
        Request::LogAccess { .. } => Response::error("LogAccess is not implemented"),
        Request::Flush => {
            let meta = SandboxMeta {
                shm_name: state.shm_name.clone(),
                abi_version: state.abi_version,
                next_id: state.objects.next_id(),
            };
            let fuse_map = FuseMap {
                entries: state
                    .fuse_map
                    .iter()
                    .map(|kv| (kv.key().clone(), kv.value().clone()))
                    .collect::<HashMap<_, _>>(),
            };

            if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map, &state.path_tree) {
                return Response::error(e.to_string());
            }
            Response::Flush
        }
    }
}
