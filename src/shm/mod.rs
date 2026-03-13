pub mod mutex;
pub mod region;
pub mod state;

pub use mutex::{adopt_mutex_after_fork, MutexError, ShmGuard};
pub use region::{ShmError, ShmRegion};
pub use state::{ShmState, ShmStateLayout, SHM_LAYOUT_SIZE};

#[cfg(test)]
mod tests {
    use std::thread;

    use super::mutex::{adopt_mutex_after_fork, ShmGuard};
    use super::{ShmRegion, ShmState, ShmStateLayout, SHM_LAYOUT_SIZE};
    use serial_test::serial;

    fn cleanup_shm(name: &str) {
        let _ = ShmRegion::unlink(name);
    }

    #[test]
    #[serial]
    fn test_shm_region_create_and_open() {
        let name = "/cas_test_create_open";
        cleanup_shm(name);

        {
            let region = ShmRegion::create(name, SHM_LAYOUT_SIZE).expect("create shm");
            assert_eq!(region.size(), SHM_LAYOUT_SIZE);
            assert_eq!(region.name(), name);
        }

        {
            let region = ShmRegion::open(name, SHM_LAYOUT_SIZE).expect("open shm");
            assert_eq!(region.size(), SHM_LAYOUT_SIZE);
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_shm_state_create() {
        let name = "/cas_test_state_create";
        cleanup_shm(name);

        {
            let state = ShmState::create(name).expect("create shm state");
            assert_eq!(state.name(), name);
            assert_eq!(state.state().get_running_count(), 0);
            assert!(!state.state().is_socket_ready());
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_shm_state_increment_decrement() {
        let name = "/cas_test_inc_dec";
        cleanup_shm(name);

        {
            let state = ShmState::create(name).expect("create shm state");

            let prev = state.state().increment_running_count();
            assert_eq!(prev, 0);
            assert_eq!(state.state().get_running_count(), 1);

            let prev = state.state().increment_running_count();
            assert_eq!(prev, 1);
            assert_eq!(state.state().get_running_count(), 2);

            let prev = state.state().decrement_running_count();
            assert_eq!(prev, 2);
            assert_eq!(state.state().get_running_count(), 1);
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_shm_state_socket_ready() {
        let name = "/cas_test_socket_ready";
        cleanup_shm(name);

        {
            let state = ShmState::create(name).expect("create shm state");

            assert!(!state.state().is_socket_ready());

            state.state().set_socket_ready(true);
            assert!(state.state().is_socket_ready());

            state.state().set_socket_ready(false);
            assert!(!state.state().is_socket_ready());
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_shm_guard_lock_unlock() {
        let name = "/cas_test_guard";
        cleanup_shm(name);

        {
            let mut state = ShmState::create(name).expect("create shm state");

            let mutex_ptr = state.state_mut().mutex_ptr();

            unsafe {
                libc::pthread_mutex_init(mutex_ptr, std::ptr::null());
            }

            {
                unsafe {
                    let state_ptr = state.state_mut() as *mut ShmStateLayout;
                    let _guard = ShmGuard::new(mutex_ptr, state_ptr).expect("lock mutex");
                }
            }

            unsafe {
                libc::pthread_mutex_destroy(mutex_ptr);
            }
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_adopt_mutex_after_fork() {
        let name = "/cas_test_fork";
        cleanup_shm(name);

        let mut state = ShmState::create(name).expect("create shm state");

        let mutex_ptr = state.state_mut().mutex_ptr();

        unsafe {
            libc::pthread_mutex_init(mutex_ptr, std::ptr::null());
        }

        let pid = unsafe { libc::fork() };

        if pid == 0 {
            unsafe {
                let result = adopt_mutex_after_fork(state.state_mut());
                if result.is_err() {
                    libc::_exit(1);
                }

                state.state_mut().set_socket_ready(true);

                libc::pthread_mutex_destroy(state.state_mut().mutex_ptr());
            }
            unsafe {
                libc::_exit(0);
            }
        } else {
            let mut status: i32 = 0;
            unsafe {
                libc::waitpid(pid, &mut status as *mut i32, 0);
            }

            assert_eq!(status, 0, "child process exited successfully");

            unsafe {
                libc::pthread_mutex_destroy(mutex_ptr);
            }
        }

        cleanup_shm(name);
    }

    #[test]
    #[serial]
    fn test_concurrent_access() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let name = "/cas_test_concurrent";
        cleanup_shm(name);

        let state = ShmState::create(name).expect("create shm state");

        let state_ptr = Arc::new(AtomicUsize::new(
            state.state() as *const ShmStateLayout as usize
        ));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ptr = Arc::clone(&state_ptr);
                thread::spawn(move || {
                    let raw = ptr.load(std::sync::atomic::Ordering::SeqCst);
                    let state = unsafe { &mut *(raw as *mut ShmStateLayout) };
                    for _ in 0..100 {
                        let _prev = state.increment_running_count();
                        thread::yield_now();
                        let _prev = state.decrement_running_count();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let final_count = state.state().get_running_count();
        assert_eq!(final_count, 0);

        cleanup_shm(name);
    }
}
