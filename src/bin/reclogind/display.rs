//! The kernel's display device: finding it, and being its master while the
//! login console is shown.
//!
//! - At start-up its driver may still be loading: [`Display::open_waiting`]
//!   looks again every second for up to 30 seconds before it gives up, and
//!   nothing counts as a greeter failure meanwhile.
//! - Opening the device while nobody is its master makes the opener the
//!   master. When reclogind starts behind a user's console it lets go at
//!   once ([`Display::give_up_quietly`]), so that a desktop started from that
//!   console can still have the display.
//! - When taking the display fails while the login console is shown, the
//!   device is closed and opened once more ([`Display::reopen`]) before the
//!   greeter is started or resumed (`console.rs`).

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
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

/// How long start-up waits for a display device to appear, looking once a
/// second.
const APPEAR_WITHIN: u32 = 30;

/// Why no display could be opened.
enum Missing {
    /// No `/dev/dri/card*` at all (yet).
    NoDevice,
    Unusable(String),
}

impl Display {
    /// [`Display::open`], but when there is no display device at all yet
    /// (its driver still loading at boot), look again every second for up
    /// to 30 seconds.
    pub fn open_waiting() -> Result<Display> {
        for waited in 0..=APPEAR_WITHIN {
            match Display::find() {
                Ok(display) => {
                    if waited > 0 {
                        log(
                            "reclogind",
                            format_args!("{} appeared after {waited} s", display.path),
                        );
                    }
                    return Ok(display);
                }
                Err(Missing::NoDevice) if waited < APPEAR_WITHIN => {
                    if waited == 0 {
                        log(
                            "reclogind",
                            "no display device yet; waiting up to 30 seconds for one",
                        );
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                Err(Missing::NoDevice) => {
                    bail!("no display device (/dev/dri/card*) within {APPEAR_WITHIN} seconds")
                }
                Err(Missing::Unusable(why)) => bail!("no usable display device: {why}"),
            }
        }
        unreachable!("the last pass returns")
    }

    /// The first `/dev/dri/card*` that can set modes, preferring one with a
    /// connected screen.
    pub fn open() -> Result<Display> {
        Display::find().map_err(|missing| match missing {
            Missing::NoDevice => anyhow!("no display device (/dev/dri/card*)"),
            Missing::Unusable(why) => anyhow!("no usable display device: {why}"),
        })
    }

    fn find() -> Result<Display, Missing> {
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
            None if tried.is_empty() => Err(Missing::NoDevice),
            None => Err(Missing::Unusable(tried.join("; "))),
        }
    }

    /// Close the device and open it (or the best one there is now) again.
    /// On failure the device held stays.
    pub fn reopen(&mut self) -> Result<()> {
        let fresh = Display::open()?;
        log(
            "reclogind",
            format_args!("closed {} and opened {}", self.path, fresh.path),
        );
        *self = fresh;
        Ok(())
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

    /// Let go of the master role if opening the device gave it to us; not
    /// being the master is no error here.
    pub fn give_up_quietly(&self) {
        let _ = self.release_master_lock();
    }
}
