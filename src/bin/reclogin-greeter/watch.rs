//! What the greeter does besides drawing the form and passing keys on: the
//! on-screen keyboard's keys and touch, the other screens, idle (screens off,
//! a typed password cleared) and noticing a plugged or removed screen. All of
//! it is `impl Greeter`, kept out of `main.rs`.

use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::MessageKind;
use tiny_skia::Pixmap;

use crate::form::{Action, Focus, Key};
use crate::input::{Happening, TouchPhase};
use crate::osk::{self, Effect, KeyId};
use crate::screens::{self, Target};
use crate::{Greeter, WHO};

/// A typed password or user name is cleared after this long without input.
const CLEAR_AFTER: Duration = Duration::from_secs(60);
/// How often the connectors are looked at again.
const RESCAN_EVERY: Duration = Duration::from_secs(3);

pub struct Watch {
    /// The last key, pointer motion, click or touch.
    last_input: Instant,
    /// The screens are switched off.
    pub asleep: bool,
    /// A physical key was pressed since the greeter started: there is a
    /// keyboard, so a touch does not open the on-screen one by itself.
    physical_key: bool,
    /// The press now being handled is a touch.
    touch_down: bool,
    rescan_at: Instant,
    rescan_failed: bool,
}

impl Watch {
    pub fn new() -> Watch {
        let now = Instant::now();
        Watch {
            last_input: now,
            asleep: false,
            physical_key: false,
            touch_down: false,
            rescan_at: now + RESCAN_EVERY,
            rescan_failed: false,
        }
    }

    /// The display left (a console switch): nothing is switched off any more.
    pub fn paused(&mut self) {
        self.asleep = false;
    }

    /// The display is back: a fresh idle period.
    pub fn resumed(&mut self) {
        self.asleep = false;
        self.last_input = Instant::now();
    }
}

impl Greeter {
    /// What to do with a happening before the form sees it: note the input,
    /// wake the screens (the input that woke them goes no further), turn
    /// touch into the pointer's clicks.
    pub(crate) fn admit(&mut self, happening: Happening) -> Option<Happening> {
        // Caps Lock is a state of the keyboard; keep it right even when the
        // key only woke the screens.
        if let Happening::CapsLock(on) = &happening {
            self.form.caps_lock = *on;
        }
        self.watch.last_input = Instant::now();
        if self.watch.asleep {
            self.watch.asleep = false;
            self.screen.set_power(true);
            self.input.cancel_repeat();
            self.dirty = true;
            return None;
        }
        match happening {
            Happening::Key(_) => {
                self.watch.physical_key = true;
                Some(happening)
            }
            Happening::Pointer(..) | Happening::Press(..) => Some(happening),
            Happening::Release => {
                self.form.keyboard.release();
                self.dirty = true;
                None
            }
            Happening::Touch(TouchPhase::Down, x, y) => {
                self.watch.touch_down = true;
                self.soft_pointer = None;
                self.screen.hide_pointer();
                Some(Happening::Press(x, y))
            }
            Happening::Touch(TouchPhase::Move, ..) => None,
            Happening::Touch(TouchPhase::Up, ..) => {
                self.form.keyboard.release();
                self.dirty = true;
                None
            }
            Happening::CapsLock(_) => Some(happening),
        }
    }

    /// After a click or touch hit `target`: a touch on a text field opens
    /// the on-screen keyboard when no physical key was ever pressed.
    pub(crate) fn after_press(&mut self, target: Target, touched: bool) {
        if touched
            && !self.watch.physical_key
            && matches!(target, Target::Password | Target::UserName)
            && matches!(self.form.focus, Focus::Password | Focus::UserName)
        {
            osk::open(&mut self.form, &self.state);
        }
    }

    /// Whether the press being handled came from a finger (read once).
    pub(crate) fn take_touch_down(&mut self) -> bool {
        std::mem::take(&mut self.watch.touch_down)
    }

    /// A key of the on-screen keyboard was pressed.
    pub(crate) fn press_key(&mut self, id: KeyId) {
        let can_switch = self.state.layouts.len() > 1;
        match self.form.keyboard.press(id, can_switch) {
            Effect::None => {}
            Effect::NextLayout => self.act(Action::NextLayout),
            Effect::Type(key) => {
                // Enter on a key of the keyboard answers the field being
                // typed in, wherever Tab left the focus.
                if key == Key::Enter
                    && !matches!(self.form.focus, Focus::UserName | Focus::Password)
                {
                    self.form.focus = Focus::Password;
                }
                if let Some(action) = self.form.key(key, &self.state) {
                    self.act(action);
                }
            }
        }
    }

    /// The keys' letters follow the layout: read the keymap again when the
    /// layout changed.
    pub(crate) fn sync_keyboard(&mut self) {
        let serial = self.input.layout_serial();
        if self.form.keyboard.table_serial != Some(serial) {
            self.form.keyboard.table = self.input.key_table();
            self.form.keyboard.table_serial = Some(serial);
        }
    }

    /// A frame was drawn. A key released before it showed pressed is let go
    /// now, and that takes one more frame.
    pub(crate) fn after_draw(&mut self) {
        if self.form.keyboard.drawn() {
            self.dirty = true;
        }
    }

    /// Draw every screen but the primary one: once, and again only when the
    /// brand or the size changed (or the screen is new).
    pub(crate) fn draw_others(&mut self) {
        for (index, (w, h)) in self.screen.secondaries() {
            let drawn = (self.look.brand.name.clone(), w, h);
            if !self.screen.needs_picture(index, &drawn) {
                continue;
            }
            let Some(mut picture) = Pixmap::new(w, h) else {
                continue;
            };
            {
                let mut painter = self.look.begin(&mut picture);
                screens::second::draw(&mut painter);
            }
            self.screen.show_other(index, drawn, &picture);
        }
        self.screen.reshow_others();
    }

    /// When something here needs the loop to wake up.
    pub(crate) fn watch_due(&self) -> Instant {
        let mut due = self.watch.rescan_at;
        let idle = self.state.idle_seconds.filter(|n| *n > 0);
        if let (Some(n), false) = (idle, self.watch.asleep) {
            due = due.min(self.watch.last_input + Duration::from_secs(n));
        }
        if self.typed_something() {
            due = due.min(self.watch.last_input + CLEAR_AFTER);
        }
        due
    }

    fn typed_something(&self) -> bool {
        !self.form.password.is_empty() || !self.form.user_name.is_empty()
    }

    /// Idle: switch the screens off after `idle_seconds`, clear what was
    /// typed after a minute, look at the connectors every few seconds.
    pub(crate) fn tick(&mut self) {
        if self.paused {
            return;
        }
        let now = Instant::now();
        let idle = now.saturating_duration_since(self.watch.last_input);
        if !self.watch.asleep {
            if let Some(n) = self.state.idle_seconds.filter(|n| *n > 0) {
                if idle >= Duration::from_secs(n) {
                    self.watch.asleep = true;
                    self.screen.set_power(false);
                }
            }
        }
        if idle >= CLEAR_AFTER && self.typed_something() {
            let both = !self.form.password.is_empty() && !self.form.user_name.is_empty();
            let name_only = self.form.password.is_empty();
            self.form.clear_password();
            self.form.user_name.clear();
            let what = if both {
                "The user name and password were"
            } else if name_only {
                "The user name was"
            } else {
                "The password was"
            };
            self.form.notice = Some((
                MessageKind::Info,
                format!("{what} cleared after a minute without input."),
            ));
            self.dirty = true;
        }
        if now >= self.watch.rescan_at {
            self.watch.rescan_at = now + RESCAN_EVERY;
            self.rescan();
        }
    }

    /// Look at the connectors: a screen plugged in gets the picture, one
    /// removed is let go, and when the primary went the login moves to the
    /// next one (the frame and the pointer's box follow its size).
    fn rescan(&mut self) {
        let changes = match self.screen.rescan(false) {
            Ok(changes) => {
                self.watch.rescan_failed = false;
                changes
            }
            Err(e) => {
                if !self.watch.rescan_failed {
                    log(WHO, format_args!("looking at the screens: {e:#}"));
                }
                self.watch.rescan_failed = true;
                return;
            }
        };
        if changes.primary {
            let (w, h) = self.screen.size();
            if let Some(frame) = Pixmap::new(w, h) {
                self.frame = frame;
            }
            self.input.set_size((w, h));
            self.soft_pointer = None;
            self.hits.clear();
        }
        if changes.primary || changes.others {
            self.dirty = true;
        }
    }
}
