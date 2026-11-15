//! The screens. Each one is a function from (brand, state, form, clock,
//! size) to pixels: it draws through a [`Painter`](crate::paint::Painter)
//! that already holds the brand and the frame's size, and returns nothing
//! but the areas a pointer may hit. No screen touches a device.
//!
//! The normal login screen ([`login`]) also draws a failed attempt (the
//! wrong-password design): PAM's messages and the error line under the
//! field. While PAM asks further questions (an expired password) the
//! [`conversation`] screen is drawn instead. [`other`] is "Other user…" and
//! the screen of a system with no listed account; [`prompt`] is the guest and
//! returning to a running session; [`shutdown`] is the question before a
//! restart or shut down while others are logged in. [`keyboard`] is the login
//! with the on-screen keyboard open; [`second`] is what the screens other than
//! the primary one show. Later stages add the text-login fallback beside them. The open session menu is drawn over
//! whichever screen it belongs to.

pub mod conversation;
pub mod keyboard;
pub mod login;
pub mod other;
pub mod prompt;
pub mod second;
pub mod shutdown;

use crate::form::Screen;
use crate::paint::Painter;

/// Draw the screen the form is on.
pub fn draw(p: &mut Painter, v: &login::View) {
    if let Some(action) = v.form.question {
        shutdown::draw(p, v, action);
        return;
    }
    if crate::osk::visible(v.form, v.state) {
        keyboard::draw(p, v);
        return;
    }
    if v.form.talking() {
        conversation::draw(p, v);
        return;
    }
    match v.form.screen(v.state) {
        Screen::Login => login::draw(p, v),
        Screen::Return | Screen::Guest => prompt::draw(p, v),
        Screen::Other | Screen::NoUsers => other::draw(p, v),
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
    UserName,
    Password,
    ShowPassword,
    LogIn,
    /// "Cancel" and "Someone else" while PAM asks further questions.
    Cancel,
    /// "… anyway" on the power question.
    Confirm,
    Session,
    /// One entry of the open session menu (index into `State::sessions`).
    SessionOption(usize),
    /// The open session menu's own area: a click there changes nothing.
    Menu,
    Layout,
    OnScreenKeyboard,
    /// A key of the on-screen keyboard.
    Key(crate::osk::KeyId),
    Power(PowerAction),
    /// "End the guest session" on the screen of a running guest session.
    EndGuest,
}

/// The time as the screen shows it.
pub struct Clock {
    pub time: String,
    pub date: String,
    pub greeting: &'static str,
    /// The instant these were made from, for "since 19:10" on a screen.
    pub now: DateTime<Local>,
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
            now,
        }
    }
}
