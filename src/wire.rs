//! How a message and its file handles cross the socket between the two
//! programs.
//!
//! The socket is a Unix `SOCK_SEQPACKET` pair: every packet is one message
//! (one JSON object and a newline), so message boundaries are kept and the
//! file handles sent with a packet (`SCM_RIGHTS`) belong to that message.
//! Both the sending and the receiving buffer are reserved at full size up
//! front and overwritten after use, because a packet may carry a secret.

use std::io;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::de::DeserializeOwned;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::protocol::Framed;

/// The largest message, newline included.
pub const MESSAGE_MAX: usize = 128 * 1024;

/// The most handles one message may carry.
const HANDLES_MAX: usize = 4;

/// One end of the socket pair.
pub struct Channel {
    fd: OwnedFd,
}

impl Channel {
    /// A connected pair; both ends are closed on exec.
    pub fn pair() -> io::Result<(Channel, Channel)> {
        let mut fds = [0 as RawFd; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok((Channel { fd: a }, Channel { fd: b }))
    }

    pub fn from_fd(fd: OwnedFd) -> Channel {
        Channel { fd }
    }

    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    /// Send `message` with `handles` beside it. The handle count must be the
    /// one the message announces.
    pub fn send<M: Serialize + Framed>(
        &self,
        message: &M,
        handles: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        if handles.len() != message.handles() || handles.len() > HANDLES_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "message {} announces {} handles, {} given",
                    message.kind(),
                    message.handles(),
                    handles.len()
                ),
            ));
        }
        let mut line = Zeroizing::new(Vec::with_capacity(MESSAGE_MAX + 1));
        serde_json::to_writer(&mut *line, message).map_err(io::Error::other)?;
        if line.len() >= MESSAGE_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("message {} is too long", message.kind()),
            ));
        }
        line.push(b'\n');

        let mut iov = libc::iovec {
            iov_base: line.as_ptr() as *mut libc::c_void,
            iov_len: line.len(),
        };
        let mut control = Control([0u8; control_space(HANDLES_MAX)]);
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        if !handles.is_empty() {
            let payload = mem::size_of_val(handles.first().unwrap()) * handles.len();
            let space = unsafe { libc::CMSG_SPACE(payload as u32) } as usize;
            header.msg_control = control.0.as_mut_ptr().cast();
            header.msg_controllen = space as _;
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&header);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(payload as u32) as _;
                let data = libc::CMSG_DATA(cmsg) as *mut RawFd;
                for (i, handle) in handles.iter().enumerate() {
                    data.add(i).write_unaligned(handle.as_raw_fd());
                }
            }
        }
        loop {
            let sent = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
            if sent >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Receive one message and its handles. `Ok(None)` when the other end
    /// has closed. A message whose handle count differs from what it
    /// announces is an error, and the handles are closed.
    pub fn recv<M: DeserializeOwned + Framed>(&self) -> io::Result<Option<(M, Vec<OwnedFd>)>> {
        let mut buffer = Zeroizing::new(vec![0u8; MESSAGE_MAX]);
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut control = Control([0u8; control_space(HANDLES_MAX)]);
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.0.as_mut_ptr().cast();
        header.msg_controllen = control.0.len() as _;
        let got = loop {
            let got =
                unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
            if got >= 0 {
                break got as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };

        let mut handles = Vec::new();
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&header);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                    for i in 0..payload / mem::size_of::<RawFd>() {
                        handles.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&header, cmsg);
            }
        }
        if header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message or handles cut off",
            ));
        }
        if got == 0 {
            return Ok(None);
        }
        let mut line = &buffer[..got];
        if let Some(stripped) = line.strip_suffix(b"\n") {
            line = stripped;
        }
        let message: M = serde_json::from_slice(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unreadable message: {e}"),
            )
        })?;
        if handles.len() != message.handles() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "message {} came with {} handles, it announces {}",
                    message.kind(),
                    handles.len(),
                    message.handles()
                ),
            ));
        }
        Ok(Some((message, handles)))
    }

    /// Wait until a message can be read, at most `timeout_ms` (-1: forever).
    /// Returns false on timeout.
    pub fn wait_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if n >= 0 {
                return Ok(n > 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for Channel {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Room for the handles' control message, aligned as the kernel wants.
#[repr(C, align(8))]
struct Control([u8; control_space(HANDLES_MAX)]);

/// `CMSG_SPACE` for `n` handles, usable in a constant.
const fn control_space(n: usize) -> usize {
    let align = mem::size_of::<usize>();
    let header = (mem::size_of::<libc::cmsghdr>() + align - 1) & !(align - 1);
    let payload = (n * mem::size_of::<RawFd>() + align - 1) & !(align - 1);
    header + payload
}
