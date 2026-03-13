use std::path::PathBuf;
use std::sync::Mutex;

use log;
use unix_socket::{UnixListener, UnixStream};

use crate::shm::ShmState;
use crate::syncing::disk::{self, AccessLog, FuseMap, SandboxMeta};
use crate::syncing::object::ObjectStore;
use crate::syncing::proto::{Request, Response};

pub struct ServerState {
    pub objects: Mutex<ObjectStore>,
    pub fuse_map: Mutex<FuseMap>,
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

    let state = ServerState {
        objects: Mutex::new(object_store),
        fuse_map: Mutex::new(fuse_map),
        access_log: Mutex::new(access_log),
        sandbox_dir: sandbox_dir.clone(),
        shm_name: meta.shm_name.clone(),
        abi_version: meta.abi_version,
    };

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

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = handle_connection(&state, stream) {
                    log::error!("Connection error: {}", e);
                }

                let running = shm.running_count();
                if running == 0 {
                    break;
                }
            }
            Err(e) => {
                log::error!("Accept error: {}", e);
                break;
            }
        }
    }

    let meta = SandboxMeta {
        shm_name: state.shm_name.clone(),
        abi_version: state.abi_version,
        next_id: state.objects.lock().unwrap().next_id(),
    };
    let fuse_map = state.fuse_map.lock().unwrap().clone();

    if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map) {
        log::error!("Failed to flush metadata: {}", e);
    }

    let _ = std::fs::remove_file(&sock_path);

    std::process::exit(0);
}

use std::io::{Read, Write};

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
        Request::PutFile { path, data, meta } => {
            let mut objects = state.objects.lock().unwrap();
            let mut fuse_map = state.fuse_map.lock().unwrap();

            match objects.put(&data) {
                Ok(id) => {
                    let entry = crate::syncing::proto::FuseEntry {
                        id,
                        entry_type: crate::syncing::proto::EntryType::File,
                        metadata: meta,
                    };
                    fuse_map.entries.insert(path, entry);
                    Response::PutFile { id }
                }
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::PutFileMeta { path, meta } => {
            let mut fuse_map = state.fuse_map.lock().unwrap();

            if let Some(entry) = fuse_map.entries.get_mut(&path) {
                entry.metadata = meta;
                Response::PutFileMeta
            } else {
                Response::error("File not found")
            }
        }
        Request::GetFileMeta { path } => {
            let fuse_map = state.fuse_map.lock().unwrap();
            let meta = fuse_map.entries.get(&path).map(|e| e.metadata.clone());
            Response::GetFileMeta(meta)
        }
        Request::GetEntry { path } => {
            let fuse_map = state.fuse_map.lock().unwrap();
            let entry = fuse_map.entries.get(&path).cloned();
            Response::GetEntry(entry)
        }
        Request::DeleteFile { path } => {
            let mut fuse_map = state.fuse_map.lock().unwrap();
            fuse_map.entries.remove(&path);
            Response::DeleteFile
        }
        Request::RenameFile { from, to } => {
            let mut fuse_map = state.fuse_map.lock().unwrap();
            if let Some(entry) = fuse_map.entries.remove(&from) {
                fuse_map.entries.insert(to, entry);
                Response::RenameFile
            } else {
                Response::error("Source path not found")
            }
        }
        Request::PutDir { path, meta } => {
            let mut objects = state.objects.lock().unwrap();
            let mut fuse_map = state.fuse_map.lock().unwrap();

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
                    fuse_map.entries.insert(path, entry);
                    Response::PutDir
                }
                Err(e) => Response::error(e.to_string()),
            }
        }
        Request::PutWhiteout { path } => {
            let mut fuse_map = state.fuse_map.lock().unwrap();

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
            fuse_map.entries.insert(path, entry);
            Response::PutWhiteout
        }
        Request::ReadDirAll { path } => {
            let fuse_map = state.fuse_map.lock().unwrap();

            let entries: Vec<_> = fuse_map
                .entries
                .iter()
                .filter(|(p, _)| {
                    // Match entries whose parent equals the requested path.
                    p.parent() == Some(path.as_path())
                        || (path == PathBuf::from("/")
                            && p.parent().is_none()
                            && !p.as_os_str().is_empty())
                })
                .map(|(p, e)| (p.clone(), e.clone()))
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
            let fuse_map = state.fuse_map.lock().unwrap().clone();

            if let Err(e) = disk::flush(&state.sandbox_dir, &meta, &fuse_map) {
                return Response::error(e.to_string());
            }
            Response::Flush
        }
    }
}
