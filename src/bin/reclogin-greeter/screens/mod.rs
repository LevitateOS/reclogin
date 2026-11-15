//! The screens. Each one is a function from (brand, state, form, clock,
//! size) to pixels: it draws through a [`Painter`](crate::paint::Painter)
//! that already holds the brand and the frame's size, and returns nothing
//! but the areas a pointer may hit. No screen touches a device.
//!
//! The normal login screen ([`login`]) also draws a failed attempt (the
//! wrong-password design): PAM's messages and the error line under the
//! field. While PAM asks further questions (an expired password) the
//! [`conversation`] screen is drawn instead. Later stages add the others of
//! the design canvas (other user, no users, guest, returning to a session,
//! shutting down, the on-screen keyboard, the second monitor, the
//! text-login fallback) beside them.

pub mod conversation;
pub mod login;

use crate::paint::Painter;

/// Draw the screen the form is on.
pub fn draw(p: &mut Painter, v: &login::View) {
    if v.form.talking() {
        conversation::draw(p, v);
    } else {
        login::draw(p, v);
    }
}

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
    /// "Cancel" and "Someone else" while PAM asks further questions.
    Cancel,
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
