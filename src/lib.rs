//! reclogin: one login screen for LevitateOS and AcornOS.
//!
//! Two programs share this library: `reclogind` (root; consoles, devices,
//! later PAM and sessions) and `reclogin-greeter` (no rights; draws the
//! screen and reads input). What they share is here:
//!
//! - [`protocol`][]: the messages between the two programs;
//! - [`wire`][]: how a message and its file handles cross the socket;
//! - [`config`][]: `/etc/reclogin.conf`;
//! - [`brand`][]: the OS's name, palette, mark, background and fonts.
//!
//! The design is in `docs/reclogin-technical.md` of the LevitateOS
//! repository.

pub mod brand;
pub mod config;
pub mod protocol;
pub mod wire;

/// Prefix every log line with the program's name. Never pass a secret here.
///
/// A failed write is ignored rather than a panic (`eprintln!` panics): a
/// session worker whose `reclogind` is gone may have lost the reader of its
/// standard error, and must still close its session.
pub fn log(who: &str, text: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{who}: {text}");
}
