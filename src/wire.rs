//! How a message and its file handles cross the socket between the two
//! programs.
//!
//! The socket is a Unix `SOCK_SEQPACKET` pair: every packet is one message
//! (one JSON object and a newline), so message boundaries are kept and the
//! file handles sent with a packet (`SCM_RIGHTS`) belong to that message.
//! A typed secret never goes into JSON: the message `answer` announces it
//! and the next packet is the secret's raw bytes ([`Channel::send_secret`],
//! [`Channel::recv_secret`]), read straight into storage overwritten when
//! dropped. (serde reads an internally tagged message into an intermediate
//! buffer first, which it frees without overwriting.) Message buffers are
//! overwritten after use as well.

use std::io;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::de::DeserializeOwned;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::protocol::{Framed, Secret, SECRET_MAX};

/// The largest message, newline included.
pub const MESSAGE_MAX: usize = 128 * 1024;

/// The first byte of a secret's raw packet.
const SECRET_MARK: u8 = b'S';

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

    /// Make the socket non-blocking: sends that do not fit return
    /// `WouldBlock` instead of waiting (reclogind's end of the greeter's
    /// socket, so that a greeter that stops reading cannot stop reclogind).
    pub fn set_nonblocking(&self) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `message` as the bytes of one packet, newline included, after
    /// checking that `handles` handles go with it. No message holds a secret
    /// (secrets cross as raw packets, [`Channel::send_secret`]); the buffer
    /// is still overwritten when dropped.
    pub fn encode<M: Serialize + Framed>(
        message: &M,
        handles: usize,
    ) -> io::Result<Zeroizing<Vec<u8>>> {
        if handles != message.handles() || handles > HANDLES_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "message {} announces {} handles, {} given",
                    message.kind(),
                    message.handles(),
                    handles
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
        Ok(line)
    }

    /// Send `message` with `handles` beside it. The handle count must be the
    /// one the message announces.
    pub fn send<M: Serialize + Framed>(
        &self,
        message: &M,
        handles: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        let line = Channel::encode(message, handles.len())?;
        self.send_packet(&[&line], handles)
    }

    /// Send one packet made of `parts`, with `handles` beside it, in one
    /// `sendmsg`. On a non-blocking socket a packet that does not fit now is
    /// `WouldBlock` and nothing is sent.
    pub fn send_packet(&self, parts: &[&[u8]], handles: &[BorrowedFd<'_>]) -> io::Result<()> {
        if handles.len() > HANDLES_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many handles",
            ));
        }
        let mut iov: Vec<libc::iovec> = parts
            .iter()
            .map(|part| libc::iovec {
                iov_base: part.as_ptr() as *mut libc::c_void,
                iov_len: part.len(),
            })
            .collect();
        let mut control = Control([0u8; control_space(HANDLES_MAX)]);
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_iov = iov.as_mut_ptr();
        header.msg_iovlen = iov.len() as _;
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

    /// Send a secret as its own packet of raw bytes, outside JSON: one
    /// marker byte ([`SECRET_MARK`], so that an empty answer is not an empty
    /// packet, which reads as a closed socket) and the bytes. The message
    /// before it announces it (`answer`). Nothing is copied on this side.
    pub fn send_secret(&self, secret: &Secret) -> io::Result<()> {
        self.send_packet(&[&[SECRET_MARK], secret.expose()], &[])
    }

    /// Receive the raw packet [`Channel::send_secret`] sent, straight into
    /// storage that is overwritten when dropped. `Ok(None)` when the other
    /// end has closed. A packet longer than [`SECRET_MAX`] bytes, without
    /// the marker, or with handles is an error (and is overwritten too).
    pub fn recv_secret(&self) -> io::Result<Option<Secret>> {
        // The marker and SECRET_MAX bytes, and one more to see a longer one.
        let mut buffer = Zeroizing::new(vec![0u8; SECRET_MAX + 2]);
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
        // Handles have no business here: closed, and an error.
        let handles = take_handles(&header);
        if header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || !handles.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a secret packet that is too long or carries handles",
            ));
        }
        if got == 0 {
            return Ok(None);
        }
        if buffer[0] != SECRET_MARK || got - 1 > SECRET_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a secret packet",
            ));
        }
        Ok(Some(Secret::from_bytes(&buffer[1..got])))
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

        let handles = take_handles(&header);
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
        // serde's own error text is never passed on: it can quote what it
        // could not read, which may be typed text.
        let message: M = serde_json::from_slice(line).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("an unreadable message of {} bytes", line.len()),
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

/// The handles of a received packet, owned (so closed when dropped).
fn take_handles(header: &libc::msghdr) -> Vec<OwnedFd> {
    let mut handles = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(header);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..payload / mem::size_of::<RawFd>() {
                    handles.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(header, cmsg);
        }
    }
    handles
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
