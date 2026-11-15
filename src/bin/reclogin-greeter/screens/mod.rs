//! The screens. Each one is a function from (brand, state, form, clock,
//! size) to pixels: it draws through a [`Painter`](crate::paint::Painter)
//! that already holds the brand and the frame's size, and returns nothing
//! but the areas a pointer may hit. No screen touches a device.
//!
//! Stage 1 has the normal login screen. Later stages add the others of the
//! design canvas (wrong password, expired password, other user, no users,
//! guest, returning to a session, shutting down, the on-screen keyboard,
//! the second monitor, the text-login fallback) beside it.

pub mod login;

use chrono::{DateTime, Datelike, Local, Timelike};
use reclogin::protocol::PowerAction;

/// What a pointer at a spot of the screen hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    User(usize),
    Guest,
    OtherUser,
    Password,
    ShowPassword,
    LogIn,
    Session,
    Layout,
    OnScreenKeyboard,
    Power(PowerAction),
}

/// The time as the screen shows it.
pub struct Clock {
    pub time: String,
    pub date: String,
    pub greeting: &'static str,
}

impl Clock {
    pub fn at(now: DateTime<Local>, hour_24: bool) -> Clock {
        let time = if hour_24 {
            now.format("%H:%M").to_string()
        } else {
            now.format("%-I:%M %p").to_string()
        };
        let date = format!("{} {} {}", now.format("%A"), now.day(), now.format("%B"));
        let greeting = match now.hour() {
            5..=11 => "Good morning",
            12..=17 => "Good afternoon",
            _ => "Good evening",
        };
        Clock {
            time,
            date,
            greeting,
        }
    }
}
