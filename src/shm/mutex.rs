use std::sync::atomic::Ordering;

use libc::pthread_mutex_t;
use nix::errno::Errno;
use thiserror::Error;

use crate::shm::state::ShmStateLayout;

#[derive(Debug, Error)]
pub enum MutexError {
    #[error("pthread error: {0}")]
    Pthread(Errno),
    #[error("mutex already initialized")]
    AlreadyInitialized,
}

pub struct ShmGuard {
    mutex: *mut pthread_mutex_t,
    state: *mut ShmStateLayout,
}

impl Drop for ShmGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(self.mutex);
        }
    }
}

impl ShmGuard {
    pub unsafe fn new(
        mutex: *mut pthread_mutex_t,
        state: *mut ShmStateLayout,
    ) -> Result<Self, MutexError> {
        let ret = libc::pthread_mutex_lock(mutex);
        if ret != 0 {
            return Err(MutexError::Pthread(Errno::from_raw(ret)));
        }
        Ok(Self { mutex, state })
    }

    pub fn set_socket_ready(&mut self, ready: bool) {
        unsafe {
            (*self.state).set_socket_ready(ready);
        }
    }

    pub fn get_running_count(&self) -> u32 {
        unsafe { (*self.state).running_count.load(Ordering::Acquire) }
    }

    /// Increments the running count and returns the previous value.
    pub fn increment(&mut self) -> u32 {
        unsafe { (*self.state).running_count.fetch_add(1, Ordering::AcqRel) }
    }

    pub fn decrement(&mut self) {
        unsafe {
            (*self.state).running_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub fn is_socket_ready(&self) -> bool {
        unsafe { (*self.state).is_socket_ready() }
    }

    /// discard the lock without unlocking
    pub fn disarm(self) {
        std::mem::forget(self);
    }
}

pub unsafe fn adopt_mutex_after_fork(state: &mut ShmStateLayout) -> Result<(), MutexError> {
    let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();

    let init_attr = libc::pthread_mutexattr_init(&mut attr);
    if init_attr != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(init_attr)));
    }

    let set_shared = libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED);
    if set_shared != 0 {
        libc::pthread_mutexattr_destroy(&mut attr);
        return Err(MutexError::Pthread(Errno::from_raw(set_shared)));
    }

    let mutex = state.mutex_ptr();
    let _ = libc::pthread_mutex_destroy(mutex);

    let init_mutex = libc::pthread_mutex_init(mutex, &attr);
    libc::pthread_mutexattr_destroy(&mut attr);

    if init_mutex != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(init_mutex)));
    }

    Ok(())
}

unsafe impl Send for ShmGuard {}
unsafe impl Sync for ShmGuard {}
