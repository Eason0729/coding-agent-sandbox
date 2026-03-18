use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use dashmap::DashMap;
use log;
use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult};
use std::os::unix::net::{UnixListener, UnixStream};

use crate::syncing::disk::{self, AccessLog, FuseMap, SandboxMeta};
use crate::syncing::object::ObjectStore;
use crate::syncing::proto::{Request, Response};

const READY_TIMEOUT: Duration = Duration::from_secs(15);

pub fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
        .max(1)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::default_worker_count;
    use crate::syncing::proto::Request;

    #[test]
    fn default_worker_count_is_bounded() {
        let n = default_worker_count();
        assert!((1..=4).contains(&n));
    }

    #[test]
    fn thread_per_connection_allows_concurrent_clients() {
        let td = tempdir().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let listener = UnixListener::bind(&sock).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");

        let active_requests = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let active_clone = Arc::clone(&active_requests);
        let running_clone = Arc::clone(&running);

        let _handle = thread::spawn(move || {
            while running_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((_stream, _)) => {
                        // Just accept connections
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let idle_count = 4;
        let mut idle_conns = Vec::new();
        for _ in 0..idle_count {
            let conn = UnixStream::connect(&sock).expect("connect idle");
            idle_conns.push(conn);
        }

        let active_clone2 = Arc::clone(&active_clone);
        let _active_conn = thread::spawn(move || {
            let mut conn = UnixStream::connect(&sock).expect("connect active");
            active_clone2.fetch_add(1, Ordering::SeqCst);

            let req = postcard::to_allocvec(&Request::GetEntry {
                path: PathBuf::from("/test"),
            })
            .unwrap();
            let len = (req.len() as u32).to_le_bytes();
            conn.write_all(&len).unwrap();
            conn.write_all(&req).unwrap();
            conn.flush().unwrap();

            let mut len_buf = [0u8; 4];
            conn.read_exact(&mut len_buf).unwrap();
        });

        thread::sleep(Duration::from_millis(100));

        assert!(
            active_requests.load(Ordering::SeqCst) > 0,
            "active request should have been served despite idle connections"
        );

        running.store(false, Ordering::Relaxed);
        drop(idle_conns);
        thread::sleep(Duration::from_millis(50));
    }
}

/// Mutable server state shared across worker threads.
///
/// This state intentionally separates high-contention data:
/// - object store mutations behind one mutex,
/// - path map in `DashMap` for concurrent row access,
/// - append-only access log behind one mutex.
pub struct ServerState {
    pub objects: Mutex<ObjectStore>,
    pub fuse_map: DashMap<PathBuf, crate::syncing::proto::FuseEntry>,
    pub access_log: Mutex<AccessLog>,
    pub sandbox_dir: PathBuf,
    pub shm_name: String,
    pub abi_version: u32,
}

/// Shutdown polling contract owned by `run.rs`.
///
/// The syncing daemon does not know SHM details. It only asks whether shutdown
/// should happen; if yes, the caller executes server-finalization callback while
/// preserving its own lock/transition invariants.
pub trait PollLock {
    fn poll_shutdown<F>(&mut self, on_shutdown: F) -> bool
    where
        F: FnOnce();
}

/// Fork and run syncing server; return when socket is ready.
pub fn fork_and_run<P, F>(sandbox_dir: PathBuf, mut poll_lock: P, on_ready: F) -> nix::Result<()>
where
    P: PollLock,
    F: FnOnce(),
{
    let (mut parent_sock, mut child_sock) = UnixStream::pair().map_err(|_| Errno::EIO)?;
    parent_sock.set_nonblocking(true).map_err(|_| Errno::EIO)?;

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            drop(child_sock);

            let deadline = std::time::Instant::now() + READY_TIMEOUT;
            let mut ready = [0u8; 1];
            log::debug!("sync.start event=parent_wait_ready");

            loop {
                match parent_sock.read(&mut ready) {
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
                thread::sleep(Duration::from_millis(10));
            }
        }
        ForkResult::Child => {
            drop(parent_sock);
            run(
                sandbox_dir,
                move || {
                    let _ = child_sock.write_all(&[1]);
                    let _ = child_sock.flush();
                },
                &mut poll_lock,
            );
        }
    }

    Ok(())
}

/// Build a flush snapshot from concurrent in-memory structures.
fn snapshot_state(state: &ServerState) -> (SandboxMeta, FuseMap) {
    let meta = SandboxMeta {
        shm_name: state.shm_name.clone(),
        abi_version: state.abi_version,
        next_id: state.objects.lock().unwrap().next_id(),
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

/// Load persisted state and build in-memory daemon structures.
fn build_server_state(sandbox_dir: &Path) -> std::result::Result<Arc<ServerState>, String> {
    let (meta, fuse_map) = disk::load(&sandbox_dir.to_path_buf()).map_err(|e| e.to_string())?;

    let objects_dir = sandbox_dir.join(".sandbox").join("data").join("objects");
    let object_store = ObjectStore::new(objects_dir, meta.next_id);

    let log_path = sandbox_dir.join(".sandbox").join("data").join("access.log");
    let access_log = AccessLog::open(&log_path).map_err(|e| e.to_string())?;

    Ok(Arc::new(ServerState {
        objects: Mutex::new(object_store),
        fuse_map: DashMap::from_iter(fuse_map.entries),
        access_log: Mutex::new(access_log),
        sandbox_dir: sandbox_dir.to_path_buf(),
        shm_name: meta.shm_name,
        abi_version: meta.abi_version,
    }))
}

/// Bind daemon socket and apply socket file permissions.
fn bind_listener(sock_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(sock_path);

    let listener = UnixListener::bind(sock_path)?;
    std::fs::set_permissions(
        sock_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Finalize daemon state and remove socket path.
fn flush_and_remove_socket(state: &ServerState, sock_path: &Path) {
    let (meta, fuse_map) = snapshot_state(state);
    if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map) {
        log::error!("sync.lifecycle event=flush_failed error={e}");
    }
    let _ = std::fs::remove_file(sock_path);
}

/// Accept loop with thread-per-connection model.
/// Each incoming connection gets its own dedicated thread, ensuring idle persistent
/// connections never block other connections from making progress.
fn run_accept_loop<P: PollLock>(
    listener: &UnixListener,
    state: &Arc<ServerState>,
    sock_path: &Path,
    poll_lock: &mut P,
    shutdown_flag: &Arc<AtomicBool>,
) -> bool {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let state_ref = Arc::clone(state);
                let shutdown_flag_ref = Arc::clone(shutdown_flag);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(&state_ref, stream) {
                        log::error!("sync.conn event=serve_failed error={e}");
                    }
                    if shutdown_flag_ref.load(Ordering::Acquire) {
                        log::debug!("sync.conn event=connection_shutdown");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                log::error!("sync.accept event=error error={e}");
                return false;
            }
        }

        if poll_lock.poll_shutdown(|| flush_and_remove_socket(state.as_ref(), sock_path)) {
            return true;
        }
    }
}

/// Main daemon loop for the syncing process.
pub fn run<P: PollLock>(sandbox_dir: PathBuf, ready: impl FnOnce(), poll_lock: &mut P) {
    let state = match build_server_state(&sandbox_dir) {
        Ok(v) => v,
        Err(e) => {
            log::error!("sync.start event=state_init_failed error={e}");
            std::process::exit(1);
        }
    };

    let sock_path = sandbox_dir.join(".sandbox").join("daemon.sock");
    let listener = match bind_listener(&sock_path) {
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

    let shutdown_committed =
        run_accept_loop(&listener, &state, &sock_path, poll_lock, &shutdown_flag);

    shutdown_flag.store(true, Ordering::Release);

    thread::sleep(Duration::from_millis(100));

    if !shutdown_committed {
        flush_and_remove_socket(state.as_ref(), &sock_path);
    }

    std::process::exit(0);
}

/// Serve framed request/response traffic for one connected client.
///
/// Protocol framing: little-endian u32 length prefix followed by postcard payload.
/// The loop exits cleanly on EOF and propagates parse/IO errors to the caller,
/// which logs and drops the connection.
fn handle_connection(
    state: &ServerState,
    mut stream: UnixStream,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let mut length_buf = [0u8; 4];
        match stream.read_exact(&mut length_buf) {
            Ok(()) => {}
            // Client closed the connection cleanly.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let length = u32::from_le_bytes(length_buf) as usize;

        let mut request_buf = vec![0u8; length];
        stream.read_exact(&mut request_buf)?;

        let request: Request = postcard::from_bytes(&request_buf)?;
        let response = dispatch(state, request);

        let response_data = postcard::to_allocvec(&response)?;
        let response_len = (response_data.len() as u32).to_le_bytes();
        stream.write_all(&response_len)?;
        stream.write_all(&response_data)?;
        stream.flush()?;
    }

    Ok(())
}

/// Dispatch one typed request into state mutations and typed response.
///
/// The dispatcher never panics on malformed protocol-level operations; recoverable
/// failures are mapped to `Response::Error` so clients get explicit failures.
fn dispatch(state: &ServerState, request: Request) -> Response {
    match request {
        // Object API
        Request::PutObject { data } => {
            let mut objects = state.objects.lock().unwrap();
            match objects.put(&data) {
                Ok(id) => Response::PutObject { id },
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::GetObject { id } => {
            let objects = state.objects.lock().unwrap();
            match objects.get(id) {
                Ok(data) => Response::GetObject { data },
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::GetObjectRange { id, offset, len } => {
            let objects = state.objects.lock().unwrap();
            match objects.get_range(id, offset, len as usize) {
                Ok(data) => Response::GetObjectRange { data },
                Err(e) => Response::error(e.to_string()),
            }
        }

        // File/object metadata API
        Request::PutFile { path, data, meta } => {
            let mut objects = state.objects.lock().unwrap();

            match objects.put(&data) {
                Ok(id) => {
                    let entry = crate::syncing::proto::FuseEntry {
                        id,
                        entry_type: crate::syncing::proto::EntryType::File,
                        metadata: meta,
                    };
                    state.fuse_map.insert(path, entry);
                    Response::PutFile { id }
                }
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::PatchFile {
            path,
            patches,
            truncate_to,
            meta,
        } => {
            let mut objects = state.objects.lock().unwrap();
            let Some(existing) = state.fuse_map.get(&path).map(|v| v.clone()) else {
                return Response::error("File not found");
            };

            let mut bytes = match objects.get(existing.id) {
                Ok(v) => v,
                Err(_) => Vec::new(),
            };

            for patch in patches {
                let start = patch.offset as usize;
                let end = start.saturating_add(patch.data.len());
                if end > bytes.len() {
                    bytes.resize(end, 0);
                }
                bytes[start..end].copy_from_slice(&patch.data);
            }

            if let Some(sz) = truncate_to {
                let sz = sz as usize;
                if sz < bytes.len() {
                    bytes.truncate(sz);
                } else if sz > bytes.len() {
                    bytes.resize(sz, 0);
                }
            }

            match objects.put(&bytes) {
                Ok(id) => {
                    let entry = crate::syncing::proto::FuseEntry {
                        id,
                        entry_type: existing.entry_type,
                        metadata: meta,
                    };
                    state.fuse_map.insert(path, entry);
                    Response::PatchFile { id }
                }
                Err(e) => Response::error(e.to_string()),
            }
        }

        // Namespace mutation API
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
            Response::DeleteFile
        }
        Request::RenameFile { from, to } => {
            if let Some((_, entry)) = state.fuse_map.remove(&from) {
                state.fuse_map.insert(to, entry);
                Response::RenameFile
            } else {
                Response::error("Source path not found")
            }
        }

        // Directory/whiteout API
        Request::PutDir { path, meta } => {
            let mut objects = state.objects.lock().unwrap();

            match objects.put(&[]) {
                Ok(id) => {
                    let entry = crate::syncing::proto::FuseEntry {
                        id,
                        entry_type: crate::syncing::proto::EntryType::Dir,
                        metadata: crate::syncing::proto::FileMetadata {
                            size: 0,
                            mode: meta.mode,
                            uid: meta.uid,
                            gid: meta.gid,
                            mtime: meta.mtime,
                            atime: meta.atime,
                            ctime: meta.ctime,
                        },
                    };
                    state.fuse_map.insert(path, entry);
                    Response::PutDir
                }
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::PutWhiteout { path } => {
            let entry = crate::syncing::proto::FuseEntry {
                id: 0,
                entry_type: crate::syncing::proto::EntryType::Whiteout,
                metadata: crate::syncing::proto::FileMetadata {
                    size: 0,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                    atime: 0,
                    ctime: 0,
                },
            };
            state.fuse_map.insert(path, entry);
            Response::PutWhiteout
        }

        // Enumeration/logging/flush API
        Request::ReadDirAll { path } => {
            let entries: Vec<_> = state
                .fuse_map
                .iter()
                .filter(|kv| {
                    let p = kv.key();
                    // Match entries whose parent equals the requested path.
                    p.parent() == Some(path.as_path())
                        || (path == PathBuf::from("/")
                            && p.parent().is_none()
                            && !p.as_os_str().is_empty())
                })
                .map(|kv| (kv.key().clone(), kv.value().clone()))
                .collect();

            Response::DirEntries(entries)
        }
        Request::LogAccess {
            path,
            operation,
            pid,
        } => {
            let mut access_log = state.access_log.lock().unwrap();
            match access_log.log(&path, &operation, pid) {
                Ok(()) => Response::LogAccess,
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::Flush => {
            let meta = SandboxMeta {
                shm_name: state.shm_name.clone(),
                abi_version: state.abi_version,
                next_id: state.objects.lock().unwrap().next_id(),
            };
            let fuse_map = FuseMap {
                entries: state
                    .fuse_map
                    .iter()
                    .map(|kv| (kv.key().clone(), kv.value().clone()))
                    .collect::<HashMap<_, _>>(),
            };

            if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map) {
                return Response::error(e.to_string());
            }
            Response::Flush
        }
    }
}
