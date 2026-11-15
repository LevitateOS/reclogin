//! The login console: taking it, graphics mode, console switching by
//! signal, and giving it back.
//!
//! The console is put in "process" switching mode: when someone switches
//! away the kernel sends `SIGUSR1` and waits until we say the display may
//! go ([`Console::allow_release`]); when it comes back the kernel sends
//! `SIGUSR2` and we acknowledge ([`Console::acknowledge_acquire`]). Between
//! the two the display and the input devices are given up.
//!
//! The order the two signals are read in says nothing: they are read
//! lowest number first, and a quick switch to the login console and away
//! leaves both waiting. Each is answered as the kernel requires, and then
//! the kernel is asked which console is shown ([`Daemon::console_changed`]);
//! that answer decides. Before the greeter is resumed, and after the display
//! was taken, the login console is checked to be the one shown again.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{bail, Context, Result};
use reclogin::log;
use reclogin::protocol::ToGreeter;

use crate::{Daemon, Next, WHO};

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
    /// Take console `number`: switch to it (when `switch`), put it in
    /// graphics mode with the kernel's keyboard handling off, and switch
    /// consoles by signal. With `switch` false the console shown stays shown
    /// (it belongs to a running user session); ours is taken without being
    /// shown, and the kernel tells us when it is (`ACQUIRE_SIGNAL`).
    pub fn take(number: u32, switch: bool) -> Result<Console> {
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
            number,
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
        if switch {
            console.show(number).context("showing the login console")?;
        }
        ioctl_value(&console.tty, KDSETMODE, KD_GRAPHICS).context("console to graphics mode")?;
        ioctl_value(&console.tty, KDSKBMODE, K_OFF).context("console keyboard off")?;
        log(
            "reclogind",
            format_args!(
                "took console {number} (console {} was shown{})",
                console.previous,
                if switch { "" } else { ", and stays shown" }
            ),
        );
        Ok(console)
    }

    /// Show console `number` and wait until it is shown.
    pub fn show(&self, number: u32) -> io::Result<()> {
        ioctl_value(&self.tty0, VT_ACTIVATE, number as libc::c_int)?;
        ioctl_value(&self.tty0, VT_WAITACTIVE, number as libc::c_int)
    }

    /// Whether the login console is the one shown now, asked of the kernel.
    /// `false` when it cannot be asked.
    pub fn is_shown(&self) -> bool {
        active_of(self.tty0.as_raw_fd()) == Some(self.number)
    }

    /// The console shown now, asked of the kernel.
    pub fn active(&self) -> Option<u32> {
        active_of(self.tty0.as_raw_fd())
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
        // Only from our own console: when another one is shown (a user's
        // session, say) the display is not taken away from it.
        if self.previous != 0
            && self.previous != self.number
            && active_of(self.tty0.as_raw_fd()) == Some(self.number)
        {
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
pub fn restore_raw(tty: RawFd, tty0: RawFd, keyboard_mode: libc::c_int, show: u32, own: u32) {
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
        // Only from our own console (`own`, or any when it is not known):
        // another one that is shown is left shown.
        if show != 0 && (own == 0 || active_of(tty0) == Some(own)) {
            libc::ioctl(tty0, VT_ACTIVATE as _, show as libc::c_long);
        }
    }
}

/// The console shown, asked through the open `/dev/tty0`. Only system
/// calls, so safe in a signal handler.
fn active_of(tty0: RawFd) -> Option<u32> {
    let mut state = VtStat::default();
    if unsafe { libc::ioctl(tty0, VT_GETSTATE as _, &mut state) } < 0 {
        return None;
    }
    Some(u32::from(state.v_active))
}

/// The console shown now, or `None` when it cannot be asked.
pub fn active_console() -> Option<u32> {
    let tty0 = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open("/dev/tty0")
        .ok()?;
    active_of(tty0.as_raw_fd())
}

/// Whether the console open as `tty` is in graphics mode.
pub fn graphics_mode(tty: RawFd) -> io::Result<bool> {
    let mut mode: libc::c_int = 0;
    if unsafe { libc::ioctl(tty, KDGETMODE as _, &mut mode) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode == KD_GRAPHICS)
}

impl Daemon {
    /// After the console signals waiting were answered: act on which
    /// console the kernel says is shown, never on the order they came in.
    pub(crate) fn console_changed(&mut self) -> Next {
        if !self.console.is_shown() {
            if self.shown {
                self.release_display();
            }
            return Next::Continue;
        }
        // A session is starting or runs (its console comes back), or the
        // last one is still closing (a greeter is started now): `login.rs`.
        if let Some(next) = self.login_on_acquire() {
            return next;
        }
        if self.shown {
            return Next::Continue;
        }
        self.show_greeter()
    }

    /// The login console is shown: the display taken, and the greeter given
    /// its display and state (started while the console was not shown) or
    /// resumed. Each only while the login console is still the one shown.
    pub(crate) fn show_greeter(&mut self) -> Next {
        let reopened = match self.take_display_for_greeter() {
            Some(reopened) => reopened,
            None => return Next::Continue,
        };
        self.shown = true;
        self.logind_shown();
        if reopened {
            // The greeter holds the old device: a fresh one gets the new one.
            // Its going is reclogind's doing and not counted.
            self.stop_greeter();
            self.inputs.revoke_all();
            return match self.supervisor.launch_until_up(&self.display, true) {
                Ok(greeter) => {
                    self.greeter = greeter;
                    self.login_after_handshake()
                }
                Err(reason) => self.give_up(&reason),
            };
        }
        if self.supervisor.handshake_pending {
            // Started while the console was not shown.
            return self.finish_handshake();
        }
        self.greeter.shown_again();
        self.send(&ToGreeter::Resume)
    }

    /// After a session ended, whether the login console should be shown:
    /// only when the console shown is the ended session's own, or belongs
    /// to no user session on this seat (a bare text login prompt, say). A
    /// person who moved to another session stays there. When the shown
    /// console cannot be asked, yes, as before.
    pub(crate) fn login_console_wanted(&self, ended: Option<u32>) -> bool {
        match self.console.active() {
            None => true,
            Some(active) => Some(active) == ended || !self.logind.session_on(active),
        }
    }

    /// Become the display's master for a greeter on the login console: on
    /// failure the device is closed and opened once more. `Some(reopened)`
    /// when the login console is still the one shown afterwards (taking can
    /// wait two seconds); `None` (the display given up again) when it is not.
    /// Failing to take it is logged and the greeter goes ahead: one that
    /// cannot set a mode says so in its exit status and is counted.
    pub(crate) fn take_display_for_greeter(&mut self) -> Option<bool> {
        let mut reopened = false;
        if let Err(e) = self.display.take() {
            log(
                WHO,
                format_args!(
                    "taking the display {}: {e}; opening it again",
                    self.display.path
                ),
            );
            match self.display.reopen() {
                Ok(()) => {
                    reopened = true;
                    if let Err(e) = self.display.take() {
                        log(
                            WHO,
                            format_args!("taking the display {} again: {e}", self.display.path),
                        );
                    }
                }
                Err(e) => log(WHO, format_args!("opening the display again: {e:#}")),
            }
        }
        if !self.console.is_shown() {
            log(
                WHO,
                "the login console was left while the display was being taken",
            );
            self.display.give_up_quietly();
            self.shown = false;
            return None;
        }
        Some(reopened)
    }
}
