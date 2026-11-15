//! The greeter's own part of its lock-down, done first thing, before it
//! reads anything from the socket or any input.
//!
//! The rest (the account, no capabilities, no network, limits, Landlock,
//! nothing inherited) is applied by reclogind between fork and exec
//! (`reclogind/sandbox.rs`), where the greeter cannot skip it. Here:
//!
//! - never run as root;
//! - not dumpable: exec resets this flag, so only the program itself can
//!   set it. It keeps processes of the same account from tracing it or
//!   reading its memory through `/proc`. Core dumps are off as well
//!   (reclogind set their limit to zero), since a core could hold a typed
//!   password;
//! - refuse to run without no-new-privileges, the sign that reclogind's
//!   lock-down was applied, unless started with `--no-sandbox`.

use anyhow::{bail, Result};

pub fn check(locked: bool) -> Result<()> {
    if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 } {
        bail!("the greeter must not run as root");
    }
    if !locked {
        return Ok(());
    }
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0) } != 0 {
        bail!(
            "marking the greeter not dumpable: {}",
            std::io::Error::last_os_error()
        );
    }
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        bail!("started without its lock-down: reclogind starts it, or pass --no-sandbox");
    }
    Ok(())
}
