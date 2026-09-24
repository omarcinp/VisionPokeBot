use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

/// A pseudo-terminal whose slave side stands in for the ESP32's USB serial
/// port. Optionally published under a stable path via a symlink.
pub struct VirtualSerialPort {
    pub(crate) master: File,
    // Held open so the master never sees EIO while no client is connected.
    slave: File,
    slave_path: PathBuf,
    symlink: Option<PathBuf>,
}

impl VirtualSerialPort {
    pub fn create(symlink: Option<&Path>) -> io::Result<Self> {
        // SAFETY: plain libc PTY calls; every return value is checked.
        let master = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let master = File::from_raw_fd(fd);
            if libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            master
        };
        let slave_path = {
            let mut name = [0 as libc::c_char; 128];
            // SAFETY: buffer is valid for its length.
            let rc = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            // SAFETY: ptsname_r NUL-terminates on success.
            PathBuf::from(
                unsafe { CStr::from_ptr(name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags_noctty()
            .open(&slave_path)?;
        make_raw(&slave)?;
        let symlink = match symlink {
            Some(link) => {
                if link.symlink_metadata().is_ok() {
                    std::fs::remove_file(link)?;
                }
                std::os::unix::fs::symlink(&slave_path, link)?;
                Some(link.to_path_buf())
            }
            None => None,
        };
        Ok(Self {
            master,
            slave,
            slave_path,
            symlink,
        })
    }

    /// The path a client opens (the symlink if one was requested).
    pub fn path(&self) -> &Path {
        self.symlink.as_deref().unwrap_or(&self.slave_path)
    }

    /// Serial libraries lock the port exclusively (TIOCEXCL). Because this
    /// side keeps the slave open, the lock would outlive the client and block
    /// reconnects, so it is released periodically.
    pub(crate) fn release_exclusive_lock(&self) {
        // SAFETY: TIOCNXCL takes no argument.
        unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCNXCL) };
    }
}

impl Drop for VirtualSerialPort {
    fn drop(&mut self) {
        if let Some(link) = &self.symlink {
            let _ = std::fs::remove_file(link);
        }
    }
}

fn make_raw(file: &File) -> io::Result<()> {
    // SAFETY: termios is plain data; fd is valid.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(file.as_raw_fd(), &mut termios) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut termios);
        if libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &termios) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

trait NoCtty {
    fn custom_flags_noctty(&mut self) -> &mut Self;
}

impl NoCtty for std::fs::OpenOptions {
    fn custom_flags_noctty(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_NOCTTY)
    }
}
