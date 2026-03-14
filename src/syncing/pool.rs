use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crate::syncing::client::{ClientError, SyncClient};

#[derive(Default)]
struct PoolState {
    idle: Vec<SyncClient>,
    total: usize,
}

struct PoolInner {
    sock_path: PathBuf,
    max_size: usize,
    state: Mutex<PoolState>,
    cv: Condvar,
}

#[derive(Clone)]
pub struct SyncClientPool {
    inner: Arc<PoolInner>,
}

pub struct PooledSyncClient {
    pool: SyncClientPool,
    client: Option<SyncClient>,
}

impl SyncClientPool {
    pub fn new(sock_path: PathBuf, max_size: usize) -> Self {
        let max_size = max_size.max(1);
        Self {
            inner: Arc::new(PoolInner {
                sock_path,
                max_size,
                state: Mutex::new(PoolState::default()),
                cv: Condvar::new(),
            }),
        }
    }

    pub fn checkout(&self) -> Result<PooledSyncClient, ClientError> {
        loop {
            let mut guard =
                self.inner.state.lock().map_err(|_| {
                    ClientError::Server("sync client pool mutex poisoned".to_string())
                })?;

            if let Some(client) = guard.idle.pop() {
                return Ok(PooledSyncClient {
                    pool: self.clone(),
                    client: Some(client),
                });
            }

            if guard.total < self.inner.max_size {
                guard.total += 1;
                drop(guard);

                match SyncClient::connect(&self.inner.sock_path) {
                    Ok(client) => {
                        return Ok(PooledSyncClient {
                            pool: self.clone(),
                            client: Some(client),
                        });
                    }
                    Err(err) => {
                        let mut rollback = self.inner.state.lock().map_err(|_| {
                            ClientError::Server("sync client pool mutex poisoned".to_string())
                        })?;
                        rollback.total = rollback.total.saturating_sub(1);
                        self.inner.cv.notify_one();
                        return Err(err);
                    }
                }
            }

            let waited =
                self.inner.cv.wait(guard).map_err(|_| {
                    ClientError::Server("sync client pool mutex poisoned".to_string())
                })?;
            drop(waited);
        }
    }
}

impl Deref for PooledSyncClient {
    type Target = SyncClient;

    fn deref(&self) -> &Self::Target {
        self.client
            .as_ref()
            .expect("pooled client missing during deref")
    }
}

impl DerefMut for PooledSyncClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.client
            .as_mut()
            .expect("pooled client missing during deref_mut")
    }
}

impl Drop for PooledSyncClient {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };

        if let Ok(mut guard) = self.pool.inner.state.lock() {
            guard.idle.push(client);
            self.pool.inner.cv.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::SyncClientPool;

    #[test]
    fn reuse_after_drop_uses_single_connection() {
        let td = tempdir().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let listener = UnixListener::bind(&sock).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");

        let accepts = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let accepts_t = Arc::clone(&accepts);
        let running_t = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let mut streams = Vec::new();
            while running_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        streams.push(stream);
                        accepts_t.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            streams
        });

        let pool = SyncClientPool::new(sock, 1);
        {
            let _c1 = pool.checkout().expect("first checkout");
        }
        {
            let _c2 = pool.checkout().expect("second checkout");
        }

        thread::sleep(Duration::from_millis(50));
        assert_eq!(accepts.load(Ordering::Relaxed), 1);

        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
    }

    #[test]
    fn bounded_growth_never_exceeds_max_size() {
        let td = tempdir().expect("tempdir");
        let sock = td.path().join("daemon.sock");
        let listener = UnixListener::bind(&sock).expect("bind listener");
        listener.set_nonblocking(true).expect("set nonblocking");

        let accepts = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let accepts_t = Arc::clone(&accepts);
        let running_t = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let mut streams = Vec::new();
            while running_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        streams.push(stream);
                        accepts_t.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            streams
        });

        let pool = SyncClientPool::new(sock, 2);
        let c1 = pool.checkout().expect("checkout 1");
        let c2 = pool.checkout().expect("checkout 2");

        let pool2 = pool.clone();
        let waiter = thread::spawn(move || {
            let _c3 = pool2.checkout().expect("checkout 3");
        });

        thread::sleep(Duration::from_millis(50));
        assert_eq!(accepts.load(Ordering::Relaxed), 2);

        drop(c1);
        waiter.join().expect("waiter join");
        drop(c2);

        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
    }
}
