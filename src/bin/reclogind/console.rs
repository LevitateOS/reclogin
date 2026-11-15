//! The login console: taking it, graphics mode, console switching by
//! signal, and giving it back.
//!
//! The console is put in "process" switching mode: when someone switches
//! away the kernel sends `SIGUSR1` and waits until we say the display may
//! go ([`Console::allow_release`]); when it comes back the kernel sends
//! `SIGUSR2` and we acknowledge ([`Console::acknowledge_acquire`]). Between
//! the two the display and the input devices are given up.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{bail, Context, Result};
use reclogin::log;

const KDGETMODE: libc::c_ulong = 0x4B3B;
pub const K_XLATE: libc::c_int = 1;
pub const K_UNICODE: libc::c_int = 3;
const VT_SETMODE: libc::c_ulong = 0x5602;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_RELDISP: libc::c_ulong = 0x5605;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KDGKBMODE: libc::c_ulong = 0x4B44;
const KDSKBMODE: libc::c_ulong = 0x4B45;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;
const K_OFF: libc::c_int = 4;
const VT_AUTO: libc::c_char = 0;
const VT_PROCESS: libc::c_char = 1;
const VT_ACKACQ: libc::c_int = 2;

/// The signal the kernel sends when the console is to be left.
pub const RELEASE_SIGNAL: libc::c_int = libc::SIGUSR1;
/// The signal the kernel sends when the console is shown again.
pub const ACQUIRE_SIGNAL: libc::c_int = libc::SIGUSR2;

#[repr(C)]
struct VtMode {
    mode: libc::c_char,
    waitv: libc::c_char,
    relsig: libc::c_short,
    acqsig: libc::c_short,
    frsig: libc::c_short,
}

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

pub struct Console {
    /// `/dev/tty0`, for asking which console is shown and switching.
    tty0: File,
    /// The login console.
    tty: File,
    pub number: u32,
    /// The console shown before we took ours; shown again at the end.
    pub previous: u32,
    keyboard_mode: libc::c_int,
    restored: bool,
}

fn ioctl_value(fd: &File, request: libc::c_ulong, value: libc::c_int) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, value as libc::c_long) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Console {
    /// Take console `number`: switch to it, put it in graphics mode with the
    /// kernel's keyboard handling off, and switch consoles by signal.
    pub fn take(number: u32) -> Result<Console> {
        if !(1..=63).contains(&number) {
            bail!("console {number} does not exist (1 to 63)");
        }
        let tty0 = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open("/dev/tty0")
            .context("opening /dev/tty0")?;
        let mut state = VtStat::default();
        if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, &mut state) } < 0 {
            return Err(io::Error::last_os_error()).context("asking which console is shown");
        }
        let path = format!("/dev/tty{number}");
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        let mut keyboard_mode: libc::c_int = 0;
        if unsafe { libc::ioctl(tty.as_raw_fd(), KDGKBMODE as _, &mut keyboard_mode) } < 0 {
            return Err(io::Error::last_os_error()).context("reading the console's keyboard mode");
        }
        let console = Console {
            tty0,
            tty,
            number,
            previous: u32::from(state.v_active),
            keyboard_mode,
            restored: false,
        };
        // Before anything changes: whatever ends reclogind from here on, the
        // last resort gives the console back (`supervise.rs`).
        crate::supervise::arm_last_resort(
            console.tty.as_raw_fd(),
            console.tty0.as_raw_fd(),
            keyboard_mode,
            console.previous,
        );

        let mode = VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: RELEASE_SIGNAL as libc::c_short,
            acqsig: ACQUIRE_SIGNAL as libc::c_short,
            frsig: 0,
        };
        if unsafe { libc::ioctl(console.tty.as_raw_fd(), VT_SETMODE as _, &mode) } < 0 {
            return Err(io::Error::last_os_error()).context("switching consoles by signal");
        }
        console.show(number).context("showing the login console")?;
        ioctl_value(&console.tty, KDSETMODE, KD_GRAPHICS).context("console to graphics mode")?;
        ioctl_value(&console.tty, KDSKBMODE, K_OFF).context("console keyboard off")?;
        log(
            "reclogind",
            format_args!(
                "took console {number} (console {} was shown)",
                console.previous
            ),
        );
        Ok(console)
    }

    /// Show console `number` and wait until it is shown.
    pub fn show(&self, number: u32) -> io::Result<()> {
        ioctl_value(&self.tty0, VT_ACTIVATE, number as libc::c_int)?;
        ioctl_value(&self.tty0, VT_WAITACTIVE, number as libc::c_int)
    }

    /// Ask for console `number` to be shown, without waiting.
    pub fn switch_to(&self, number: u32) -> io::Result<()> {
        ioctl_value(&self.tty0, VT_ACTIVATE, number as libc::c_int)
    }

    /// After `RELEASE_SIGNAL`, once the display and the inputs are given up.
    pub fn allow_release(&self) -> io::Result<()> {
        ioctl_value(&self.tty, VT_RELDISP, 1)
    }

    /// After `ACQUIRE_SIGNAL`.
    pub fn acknowledge_acquire(&self) -> io::Result<()> {
        ioctl_value(&self.tty, VT_RELDISP, VT_ACKACQ)
    }

    /// Text mode, the keyboard as it was, automatic switching, and the
    /// console we started from shown again. Every step is tried.
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        crate::supervise::disarm_last_resort();
        let step = |what: &str, result: io::Result<()>| {
            if let Err(e) = result {
                log(
                    "reclogind",
                    format_args!("giving the console back: {what} failed: {e}"),
                );
            }
        };
        let mode = VtMode {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        };
        let rc = unsafe { libc::ioctl(self.tty.as_raw_fd(), VT_SETMODE as _, &mode) };
        step(
            "automatic switching",
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            },
        );
        step(
            "keyboard mode",
            ioctl_value(&self.tty, KDSKBMODE, self.keyboard_mode),
        );
        step("text mode", ioctl_value(&self.tty, KDSETMODE, KD_TEXT));
        if self.previous != 0 && self.previous != self.number {
            step("showing the first console", self.show(self.previous));
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The last resort, for a panic, a fatal signal or `--reset-console`: the
/// steps of [`Console::restore`] from plain numbers, each tried, without
/// waiting for the switch. Only system calls, so safe in a signal handler.
pub fn restore_raw(tty: RawFd, tty0: RawFd, keyboard_mode: libc::c_int, show: u32) {
    let mode = VtMode {
        mode: VT_AUTO,
        waitv: 0,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };
    unsafe {
        libc::ioctl(tty, VT_SETMODE as _, &mode);
        libc::ioctl(tty, KDSKBMODE as _, keyboard_mode as libc::c_long);
        libc::ioctl(tty, KDSETMODE as _, KD_TEXT as libc::c_long);
        if show != 0 {
            libc::ioctl(tty0, VT_ACTIVATE as _, show as libc::c_long);
        }
    }
}

/// Whether the console open as `tty` is in graphics mode.
pub fn graphics_mode(tty: RawFd) -> io::Result<bool> {
    let mut mode: libc::c_int = 0;
    if unsafe { libc::ioctl(tty, KDGETMODE as _, &mut mode) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode == KD_GRAPHICS)
}
