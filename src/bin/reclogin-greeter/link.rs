//! The greeter's side of the socket to reclogind.
//!
//! libinput asks for each device synchronously, so an `OpenDevice` request
//! waits here for its answer; any other message that arrives meanwhile is
//! kept in order and handed out later by [`Link::next`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use anyhow::{bail, Context, Result};
use reclogin::protocol::{ToDaemon, ToGreeter, GREETER_SOCKET_ENV};
use reclogin::wire::Channel;

/// How long to wait for reclogind to answer a device request.
const DEVICE_TIMEOUT_MS: i32 = 5000;

pub type Incoming = (ToGreeter, Vec<OwnedFd>);

pub struct Link {
    channel: Channel,
    queue: VecDeque<Incoming>,
    next_request: u64,
    /// Open devices: our descriptor number to the request that opened it.
    devices: HashMap<RawFd, u64>,
}

impl Link {
    /// The socket reclogind passed, named by `RECLOGIN_SOCKET`.
    pub fn from_env() -> Result<Link> {
        let value = std::env::var(GREETER_SOCKET_ENV).with_context(|| {
            format!("{GREETER_SOCKET_ENV} is not set: the greeter is started by reclogind")
        })?;
        let fd: RawFd = value
            .parse()
            .with_context(|| format!("{GREETER_SOCKET_ENV}={value:?} is not a descriptor"))?;
        if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            bail!("descriptor {fd} from {GREETER_SOCKET_ENV} is not open");
        }
        let channel = Channel::from_fd(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(Link {
            channel,
            queue: VecDeque::new(),
            next_request: 1,
            devices: HashMap::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.channel.as_raw_fd()
    }

    pub fn send(&self, message: &ToDaemon) -> io::Result<()> {
        self.channel.send(message, &[])
    }

    fn recv(&self) -> Result<Incoming> {
        match self.channel.recv::<ToGreeter>()? {
            Some(incoming) => Ok(incoming),
            None => bail!("reclogind closed the socket"),
        }
    }

    /// The next message: a kept one first, else wait for one.
    pub fn next(&mut self) -> Result<Incoming> {
        match self.queue.pop_front() {
            Some(incoming) => Ok(incoming),
            None => self.recv(),
        }
    }

    /// A message kept while waiting for a device, if any.
    pub fn kept(&mut self) -> Option<Incoming> {
        self.queue.pop_front()
    }

    /// Ask reclogind to open `path`; wait for the answer. The error is an
    /// errno value for libinput.
    pub fn open_device(&mut self, path: &Path) -> Result<OwnedFd, i32> {
        let request_id = self.next_request;
        self.next_request += 1;
        if self
            .send(&ToDaemon::OpenDevice {
                request_id,
                path: path.to_path_buf(),
            })
            .is_err()
        {
            return Err(libc::EIO);
        }
        loop {
            match self.channel.wait_readable(DEVICE_TIMEOUT_MS) {
                Ok(true) => {}
                _ => return Err(libc::ETIMEDOUT),
            }
            let (message, mut handles) = self.recv().map_err(|_| libc::EIO)?;
            match message {
                ToGreeter::Device { request_id: id } if id == request_id => {
                    let fd = handles.pop().ok_or(libc::EIO)?;
                    self.devices.insert(fd.as_raw_fd(), request_id);
                    return Ok(fd);
                }
                ToGreeter::DeviceRefused {
                    request_id: id,
                    reason,
                } if id == request_id => {
                    reclogin::log(
                        "reclogin-greeter",
                        format_args!("{} refused: {reason}", path.display()),
                    );
                    return Err(libc::EACCES);
                }
                other => self.queue.push_back((other, handles)),
            }
        }
    }

    /// libinput is done with a device.
    pub fn close_device(&mut self, fd: OwnedFd) {
        if let Some(request_id) = self.devices.remove(&fd.as_raw_fd()) {
            let _ = self.send(&ToDaemon::CloseDevice { request_id });
        }
        drop(fd);
    }
}
