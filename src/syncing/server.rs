use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use dashmap::DashMap;
use log;
use std::os::unix::net::{UnixListener, UnixStream};

use crate::shm::ShmState;
use crate::syncing::disk::{self, AccessLog, FuseMap, SandboxMeta};
use crate::syncing::object::ObjectStore;
use crate::syncing::proto::{Request, Response};

pub struct ServerState {
    pub objects: Mutex<ObjectStore>,
    pub fuse_map: DashMap<PathBuf, crate::syncing::proto::FuseEntry>,
    pub access_log: Mutex<AccessLog>,
    pub sandbox_dir: PathBuf,
    pub shm_name: String,
    pub abi_version: u32,
}

pub fn run(sandbox_dir: PathBuf, mut shm: ShmState) {
    let (meta, fuse_map) = match disk::load(&sandbox_dir) {
        Ok(m) => m,
        Err(e) => {
            log::error!("Failed to load metadata: {}", e);
            std::process::exit(1);
        }
    };

    let objects_dir = sandbox_dir.join(".sandbox").join("data").join("objects");
    let object_store = ObjectStore::new(objects_dir, meta.next_id);

    let log_path = sandbox_dir.join(".sandbox").join("data").join("access.log");
    let access_log = match AccessLog::open(&log_path) {
        Ok(log) => log,
        Err(e) => {
            log::error!("Failed to open access log: {}", e);
            std::process::exit(1);
        }
    };

    let state = Arc::new(ServerState {
        objects: Mutex::new(object_store),
        fuse_map: DashMap::from_iter(fuse_map.entries),
        access_log: Mutex::new(access_log),
        sandbox_dir: sandbox_dir.clone(),
        shm_name: meta.shm_name.clone(),
        abi_version: meta.abi_version,
    });

    let sock_path = sandbox_dir.join(".sandbox").join("daemon.sock");
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).expect("Failed to create daemon socket directory");
    }

    // Remove a stale socket file left by a previous crashed run so that
    // UnixListener::bind does not fail with EADDRINUSE.
    let _ = std::fs::remove_file(&sock_path);

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Failed to bind socket: {}", e);
            std::process::exit(1);
        }
    };

    std::fs::set_permissions(
        &sock_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("Failed to set socket permissions");

    let mut guard = unsafe { shm.lock() };
    guard.set_socket_ready(true);
    drop(guard);

    if let Err(e) = listener.set_nonblocking(true) {
        log::error!("Failed to set daemon listener nonblocking: {}", e);
        std::process::exit(1);
    }

    let thread_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(4)
        .max(1);

    let (tx, rx) = mpsc::channel::<UnixStream>();
    let rx = Arc::new(Mutex::new(rx));
    let mut workers = Vec::with_capacity(thread_count);

    for _ in 0..thread_count {
        let state_ref = Arc::clone(&state);
        let rx_ref = Arc::clone(&rx);
        workers.push(thread::spawn(move || loop {
            let stream = {
                let guard = rx_ref.lock().unwrap();
                guard.recv()
            };
            let Ok(stream) = stream else {
                break;
            };
            if let Err(e) = handle_connection(state_ref.as_ref(), stream) {
                log::error!("Connection error: {}", e);
            }
        }));
    }

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = tx.send(stream);

                let running = shm.running_count();
                if running == 0 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if shm.running_count() == 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                log::error!("Accept error: {}", e);
                break;
            }
        }
    }

    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }

    let meta = SandboxMeta {
        shm_name: state.as_ref().shm_name.clone(),
        abi_version: state.as_ref().abi_version,
        next_id: state.as_ref().objects.lock().unwrap().next_id(),
    };
    let fuse_map = FuseMap {
        entries: state
            .fuse_map
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect::<HashMap<_, _>>(),
    };

    if let Err(e) = disk::flush(&state.as_ref().sandbox_dir, &meta, &fuse_map) {
        log::error!("Failed to flush metadata: {}", e);
    }

    let _ = std::fs::remove_file(&sock_path);

    std::process::exit(0);
}
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

fn dispatch(state: &ServerState, request: Request) -> Response {
    match request {
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
