//! Thin wrappers around the system calls that we need.

use std::ffi::CString;

/// A failed system call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A system call failed; `what` describes it.
    #[error("{what}: {error}")]
    Os { what: String, error: std::io::Error },
}

impl Error {
    pub fn io(what: impl Into<String>, error: std::io::Error) -> Self {
        Error::Os {
            what: what.into(),
            error,
        }
    }

    pub fn last(what: impl Into<String>) -> Self {
        Self::io(what, std::io::Error::last_os_error())
    }
}

pub fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| CString::new("?").unwrap())
}

pub fn mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: u64,
    data: Option<&str>,
) -> Result<(), Error> {
    let source_c = cstring(source);
    let target_c = cstring(target);
    let fstype_c = cstring(fstype);
    let data_c = data.map(cstring);
    // SAFETY: All pointers are NUL-terminated strings (or null).
    let result = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            if fstype.is_empty() {
                std::ptr::null()
            } else {
                fstype_c.as_ptr()
            },
            flags as libc::c_ulong,
            data_c
                .as_ref()
                .map_or(std::ptr::null(), |d| d.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(Error::last(format!("mounting {source} on {target}")));
    }
    Ok(())
}

pub fn chdir(path: &str) -> Result<(), Error> {
    // SAFETY: The path is a NUL-terminated string.
    if unsafe { libc::chdir(cstring(path).as_ptr()) } != 0 {
        return Err(Error::last(format!("chdir({path})")));
    }
    Ok(())
}

pub fn chroot(path: &str) -> Result<(), Error> {
    // SAFETY: The path is a NUL-terminated string.
    if unsafe { libc::chroot(cstring(path).as_ptr()) } != 0 {
        return Err(Error::last(format!("chroot({path})")));
    }
    Ok(())
}

/// Makes /dev/console our stdin, stdout and stderr.
pub fn open_console() {
    // SAFETY: open() and dup2() only take a path and file descriptors.
    unsafe {
        let fd = libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if fd >= 0 {
            for target in 0..3 {
                libc::dup2(fd, target);
            }
            if fd > 2 {
                libc::close(fd);
            }
        }
    }
}

pub fn set_hostname(name: &str) {
    // SAFETY: The buffer holds `len` bytes.
    unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
}

/// Syncs and powers off (through ACPI; midgard then stops).
pub fn power_off() -> ! {
    // SAFETY: These calls have no memory-safety preconditions.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Sets IFF_UP on a network interface.
pub fn interface_up(name: &str) -> Result<(), Error> {
    // SAFETY: The ioctls read and write a struct ifreq that holds the interface's name.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(Error::last("socket(AF_INET)"));
        }
        let mut request: libc::ifreq = std::mem::zeroed();
        for (target, byte) in request.ifr_name.iter_mut().zip(name.bytes()) {
            *target = byte as libc::c_char;
        }
        let mut result = libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut request);
        if result == 0 {
            request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            result = libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &request);
        }
        let error = Error::last(format!("bringing up {name}"));
        libc::close(fd);
        if result != 0 {
            return Err(error);
        }
    }
    Ok(())
}
