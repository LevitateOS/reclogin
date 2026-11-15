//! The kernel's display device: finding it, and being its master while the
//! login console is shown.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use anyhow::{bail, Result};
use drm::control::{connector, Device as ControlDevice};
use drm::Device;
use reclogin::log;

pub struct Display {
    file: File,
    pub path: String,
}

impl AsFd for Display {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Device for Display {}
impl ControlDevice for Display {}

impl Display {
    /// The first `/dev/dri/card*` that can set modes, preferring one with a
    /// connected screen.
    pub fn open() -> Result<Display> {
        let mut fallback = None;
        let mut tried = Vec::new();
        for n in 0..16 {
            let path = format!("/dev/dri/card{n}");
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tried.push(format!("{path}: {e}"));
                    continue;
                }
            };
            let display = Display {
                file,
                path: path.clone(),
            };
            let Ok(resources) = display.resource_handles() else {
                tried.push(format!("{path}: cannot set modes"));
                continue;
            };
            let connected = resources
                .connectors()
                .iter()
                .filter_map(|c| display.get_connector(*c, false).ok())
                .any(|c| c.state() == connector::State::Connected);
            if connected {
                return Ok(display);
            }
            tried.push(format!("{path}: no connected screen"));
            fallback.get_or_insert(display);
        }
        match fallback {
            Some(display) => Ok(display),
            None if tried.is_empty() => bail!("no display device (/dev/dri/card*)"),
            None => bail!("no usable display device: {}", tried.join("; ")),
        }
    }

    /// Become the display's master. Right after a console switch the last
    /// master may not have let go yet, so this tries for two seconds.
    pub fn take(&self) -> std::io::Result<()> {
        let mut result = self.acquire_master_lock();
        for _ in 0..10 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            result = self.acquire_master_lock();
        }
        result
    }

    pub fn give_up(&self) {
        if let Err(e) = self.release_master_lock() {
            log("reclogind", format_args!("giving the display up: {e}"));
        }
    }
}
