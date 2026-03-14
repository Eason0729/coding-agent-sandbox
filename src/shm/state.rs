use std::sync::atomic::AtomicU32;

use libc::pthread_mutex_t;

pub const SHM_LAYOUT_SIZE: usize = 64;

#[repr(C)]
pub struct ShmStateLayout {
    pub(crate) mutex: pthread_mutex_t,
    pub(crate) running_count: AtomicU32,
    pub(crate) socket_ready: AtomicU32,
}

impl ShmStateLayout {
    pub fn new() -> Self {
        Self {
            mutex: unsafe { std::mem::zeroed() },
            running_count: AtomicU32::new(0),
            socket_ready: AtomicU32::new(0),
        }
    }

    pub fn mutex_ptr(&self) -> *mut pthread_mutex_t {
        &self.mutex as *const pthread_mutex_t as *mut pthread_mutex_t
    }

    pub fn is_socket_ready(&self) -> bool {
        self.socket_ready.load(std::sync::atomic::Ordering::SeqCst) == 1
    }

    pub fn set_socket_ready(&self, ready: bool) {
        self.socket_ready.store(
            if ready { 1 } else { 0 },
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}

pub struct ShmState {
    region: super::region::ShmRegion,
    state: &'static mut ShmStateLayout,
}

impl ShmState {
    pub fn create(name: &str) -> Result<Self, super::region::ShmError> {
        let region = super::region::ShmRegion::create(name, SHM_LAYOUT_SIZE)?;

        let state_ptr = region.as_ptr() as *mut ShmStateLayout;
        let state = unsafe { &mut *state_ptr };

        unsafe {
            std::ptr::write(state, ShmStateLayout::new());
            let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
            let r = libc::pthread_mutexattr_init(&mut attr);
            if r != 0 {
                return Err(super::region::ShmError::Open(r));
            }
            let r = libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED);
            if r != 0 {
                libc::pthread_mutexattr_destroy(&mut attr);
                return Err(super::region::ShmError::Open(r));
            }
            let r = libc::pthread_mutex_init(state.mutex_ptr(), &attr);
            libc::pthread_mutexattr_destroy(&mut attr);
            if r != 0 {
                return Err(super::region::ShmError::Open(r));
            }
        }

        Ok(Self { region, state })
    }

    pub fn open(name: &str) -> Result<Self, super::region::ShmError> {
        let region = super::region::ShmRegion::open(name, SHM_LAYOUT_SIZE)?;

        let state_ptr = region.as_ptr() as *mut ShmStateLayout;
        let state = unsafe { &mut *state_ptr };

        Ok(Self { region, state })
    }

    pub unsafe fn lock(&self) -> super::mutex::ShmGuard {
        let state_ptr = self.state as *const ShmStateLayout as *mut ShmStateLayout;
        super::mutex::ShmGuard::new(self.state.mutex_ptr(), state_ptr)
            .expect("Failed to lock mutex")
    }
}
