use libc::{pthread_mutex_t, pthread_mutexattr_t, PTHREAD_PROCESS_SHARED};
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
        unsafe { (*self.state).get_running_count() }
    }

    pub fn decrement_running_count(&self) -> u32 {
        unsafe { (*self.state).decrement_running_count() }
    }
}

impl Drop for ShmGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(self.mutex);
        }
    }
}

unsafe impl Send for ShmGuard {}
unsafe impl Sync for ShmGuard {}

pub unsafe fn adopt_mutex_after_fork(state: &mut ShmStateLayout) -> Result<(), MutexError> {
    let mutex_ptr = state.mutex_ptr();

    let ret = libc::pthread_mutex_destroy(mutex_ptr);
    if ret != 0 && ret != libc::EBUSY {
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let mut attr: pthread_mutexattr_t = std::mem::zeroed();

    let ret = libc::pthread_mutexattr_init(&mut attr);
    if ret != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let ret = libc::pthread_mutexattr_setpshared(&mut attr, PTHREAD_PROCESS_SHARED as i32);
    if ret != 0 {
        libc::pthread_mutexattr_destroy(&mut attr);
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let ret = libc::pthread_mutex_init(mutex_ptr, &attr);
    if ret != 0 {
        libc::pthread_mutexattr_destroy(&mut attr);
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let ret = libc::pthread_mutexattr_destroy(&mut attr);
    if ret != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let ret = libc::pthread_mutex_lock(mutex_ptr);
    if ret != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    let ret = libc::pthread_mutex_unlock(mutex_ptr);
    if ret != 0 {
        return Err(MutexError::Pthread(Errno::from_raw(ret)));
    }

    Ok(())
}
