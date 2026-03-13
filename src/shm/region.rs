use std::ffi::CString;
use std::ptr::NonNull;

use thiserror::Error;

fn get_errno() -> i32 {
    errno::errno().0
}

#[derive(Debug, Error)]
pub enum ShmError {
    #[error("shm_open failed: {0}")]
    Open(i32),
    #[error("ftruncate failed: {0}")]
    Truncate(i32),
    #[error("mmap failed: {0}")]
    Mmap(i32),
    #[error("munmap failed: {0}")]
    Munmap(i32),
    #[error("shm_unlink failed: {0}")]
    Unlink(i32),
    #[error("invalid size: {0}")]
    InvalidSize(usize),
    #[error("close fd failed: {0}")]
    Close(i32),
    #[error("CString conversion failed")]
    CString(i32),
}

pub struct ShmRegion {
    ptr: NonNull<std::ffi::c_void>,
    size: usize,
    name: String,
    fd: libc::c_int,
}

impl ShmRegion {
    pub fn create(name: &str, size: usize) -> Result<Self, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidSize(size));
        }

        let cname = CString::new(name).map_err(|_| ShmError::CString(0))?;

        let fd = unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                libc::S_IRUSR | libc::S_IWUSR,
            )
        };

        if fd < 0 {
            return Err(ShmError::Open(get_errno()));
        }

        if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
            let err = get_errno();
            unsafe { libc::close(fd) };
            return Err(ShmError::Truncate(err));
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            let err = get_errno();
            unsafe { libc::close(fd) };
            return Err(ShmError::Mmap(err));
        }

        Ok(Self {
            ptr: NonNull::new(ptr).expect("mmap returned valid pointer"),
            size,
            name: name.to_string(),
            fd,
        })
    }

    pub fn open(name: &str, size: usize) -> Result<Self, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidSize(size));
        }

        let cname = CString::new(name).map_err(|_| ShmError::CString(0))?;

        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) };

        if fd < 0 {
            return Err(ShmError::Open(get_errno()));
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            let err = get_errno();
            unsafe { libc::close(fd) };
            return Err(ShmError::Mmap(err));
        }

        Ok(Self {
            ptr: NonNull::new(ptr).expect("mmap returned valid pointer"),
            size,
            name: name.to_string(),
            fd,
        })
    }

    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.ptr.as_ptr()
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn unlink(name: &str) -> Result<(), ShmError> {
        let cname = CString::new(name).map_err(|_| ShmError::CString(0))?;
        if unsafe { libc::shm_unlink(cname.as_ptr()) } < 0 {
            return Err(ShmError::Unlink(get_errno()));
        }
        Ok(())
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr(), self.size);
            libc::close(self.fd);
        }
    }
}

unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}
