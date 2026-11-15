//! Input devices opened for the greeter.
//!
//! Only `/dev/input/eventN` character devices of the input subsystem are
//! opened, read-only and non-blocking. reclogind keeps its own handle to each
//! one; when the login console is left it revokes them all, which cuts the
//! greeter's handles off too (they are the same open file), so the greeter
//! cannot read keys typed into another console.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use reclogin::log;

/// `EVIOCREVOKE`: `_IOW('E', 0x91, int)`.
const EVIOCREVOKE: libc::c_ulong = 0x4004_4591;
/// The character-device major number of the input subsystem.
const INPUT_MAJOR: u32 = 13;

#[derive(Default)]
pub struct Inputs {
    open: HashMap<u64, OwnedFd>,
}

impl Inputs {
    /// Open `path` for request `id`. Returns the handle to send.
    pub fn open(&mut self, id: u64, path: &Path) -> Result<OwnedFd, String> {
        let name = path.as_os_str().as_bytes();
        let number = name
            .strip_prefix(b"/dev/input/event")
            .ok_or("not an input event device")?;
        if number.is_empty() || !number.iter().all(u8::is_ascii_digit) || number.len() > 4 {
            return Err("not an input event device".into());
        }
        if self.open.contains_key(&id) {
            return Err("request id already used".into());
        }
        let c_path = CString::new(name).map_err(|_| "bad path")?;
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY
                    | libc::O_NONBLOCK
                    | libc::O_NOFOLLOW
                    | libc::O_NOCTTY
                    | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFCHR || libc::major(st.st_rdev) != INPUT_MAJOR {
            return Err("not an input character device".into());
        }
        let copy = fd.try_clone().map_err(|e| e.to_string())?;
        self.open.insert(id, fd);
        Ok(copy)
    }

    pub fn close(&mut self, id: u64) {
        self.open.remove(&id);
    }

    /// Cut every device off, for the greeter as well.
    pub fn revoke_all(&mut self) {
        for (id, fd) in self.open.drain() {
            if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCREVOKE as _, 0 as libc::c_long) } < 0 {
                log(
                    "reclogind",
                    format_args!("revoking input device {id}: {}", io::Error::last_os_error()),
                );
            }
        }
    }
}
