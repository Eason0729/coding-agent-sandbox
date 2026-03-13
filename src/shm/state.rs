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

    pub fn mutex_ptr(&mut self) -> *mut pthread_mutex_t {
        &mut self.mutex
    }

    pub fn running_count(&self) -> &AtomicU32 {
        &self.running_count
    }

    pub fn running_count_mut(&mut self) -> &mut AtomicU32 {
        &mut self.running_count
    }

    pub fn socket_ready(&self) -> &AtomicU32 {
        &self.socket_ready
    }

    pub fn socket_ready_mut(&mut self) -> &mut AtomicU32 {
        &mut self.socket_ready
    }

    pub fn increment_running_count(&self) -> u32 {
        self.running_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn decrement_running_count(&self) -> u32 {
        self.running_count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn get_running_count(&self) -> u32 {
        self.running_count.load(std::sync::atomic::Ordering::SeqCst)
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
        }

        Ok(Self { region, state })
    }

    pub fn open(name: &str) -> Result<Self, super::region::ShmError> {
        let region = super::region::ShmRegion::open(name, SHM_LAYOUT_SIZE)?;

        let state_ptr = region.as_ptr() as *mut ShmStateLayout;
        let state = unsafe { &mut *state_ptr };

        Ok(Self { region, state })
    }

    pub fn state(&self) -> &ShmStateLayout {
        self.state
    }

    pub fn state_mut(&mut self) -> &mut ShmStateLayout {
        self.state
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    pub fn mutex_ptr(&mut self) -> *mut pthread_mutex_t {
        self.state.mutex_ptr()
    }

    pub fn socket_ready(&self) -> bool {
        self.state.is_socket_ready()
    }

    pub fn set_socket_ready(&self, ready: bool) {
        self.state.set_socket_ready(ready);
    }

    pub fn running_count(&self) -> u32 {
        self.state.get_running_count()
    }

    pub fn increment_running_count(&self) -> u32 {
        self.state.increment_running_count()
    }

    pub fn decrement_running_count(&self) -> u32 {
        self.state.decrement_running_count()
    }

    pub unsafe fn lock(&mut self) -> super::mutex::ShmGuard {
        super::mutex::ShmGuard::new(self.state.mutex_ptr(), self.state as *mut ShmStateLayout)
            .expect("Failed to lock mutex")
    }
}
