//! Keyboard, pointer and touch through libinput, with every device opened
//! by reclogind; keys through libxkbcommon with the system's layout.
//!
//! libinput runs on its path back-end: the greeter lists `/dev/input/event*`
//! (listing needs no rights) and adds each one; libinput's open requests go
//! to reclogind as `OpenDevice`. Devices plugged in later are not picked up
//! yet. The devices are opened read-only, so the keyboard lights (Caps Lock)
//! are not switched.

use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use input::event::keyboard::{KeyState, KeyboardEventTrait};
use input::event::pointer::{ButtonState, PointerEvent};
use input::event::touch::{TouchEvent, TouchEventPosition};
use input::event::{Event, KeyboardEvent};
use input::{Libinput, LibinputInterface};
use reclogin::log;
use reclogin::protocol::Layout;
use xkbcommon::xkb;

use crate::form::Key;
use crate::link::Link;

const WHO: &str = "reclogin-greeter";
const BTN_LEFT: u32 = 0x110;
const REPEAT_DELAY: Duration = Duration::from_millis(600);
const REPEAT_INTERVAL: Duration = Duration::from_millis(40);

/// What the person did, in the greeter's terms.
pub enum Happening {
    Key(Key),
    CapsLock(bool),
    /// The pointer moved to a spot (device pixels).
    Pointer(f32, f32),
    /// A click or a touch at a spot.
    Press(f32, f32),
}

/// libinput's device opener: asks reclogind.
struct Opener(Rc<RefCell<Link>>);

impl LibinputInterface for Opener {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        self.0.borrow_mut().open_device(path)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        self.0.borrow_mut().close_device(fd);
    }
}

struct Keyboard {
    context: xkb::Context,
    state: xkb::State,
    caps: bool,
}

impl Keyboard {
    fn new(layout: Option<&Layout>) -> Result<Keyboard> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let state = xkb::State::new(&Keyboard::keymap(&context, layout)?);
        Ok(Keyboard {
            context,
            state,
            caps: false,
        })
    }

    fn keymap(context: &xkb::Context, layout: Option<&Layout>) -> Result<xkb::Keymap> {
        let (l, v, m, o) = match layout {
            Some(l) => (
                l.xkb_layout.as_str(),
                l.xkb_variant.as_str(),
                l.xkb_model.as_str(),
                l.xkb_options.as_str(),
            ),
            None => ("us", "", "", ""),
        };
        let options = (!o.is_empty()).then(|| o.to_string());
        xkb::Keymap::new_from_names(
            context,
            "evdev",
            m,
            l,
            v,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .with_context(|| format!("compiling the keyboard layout {l}({v})"))
    }

    fn set_layout(&mut self, layout: &Layout) {
        match Keyboard::keymap(&self.context, Some(layout)) {
            Ok(keymap) => {
                self.state = xkb::State::new(&keymap);
                self.caps = false;
            }
            Err(e) => log(WHO, format_args!("{e:#}")),
        }
    }

    /// Update the state for a key; on a press, what the key means.
    fn key(&mut self, code: u32, pressed: bool) -> (Option<Key>, bool) {
        let keycode = xkb::Keycode::new(code + 8);
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state.update_key(keycode, direction);
        if !pressed {
            return (None, false);
        }
        let sym = self.state.key_get_one_sym(keycode).raw();
        let ctrl = self
            .state
            .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
        let key = match sym {
            xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => Some(Key::Enter),
            xkb::keysyms::KEY_Tab => Some(Key::Tab),
            xkb::keysyms::KEY_ISO_Left_Tab => Some(Key::BackTab),
            xkb::keysyms::KEY_BackSpace => Some(Key::Backspace),
            xkb::keysyms::KEY_Escape => Some(Key::Escape),
            xkb::keysyms::KEY_Up => Some(Key::Up),
            xkb::keysyms::KEY_Down => Some(Key::Down),
            s if (xkb::keysyms::KEY_XF86Switch_VT_1..=xkb::keysyms::KEY_XF86Switch_VT_12)
                .contains(&s) =>
            {
                Some(Key::Console(s - xkb::keysyms::KEY_XF86Switch_VT_1 + 1))
            }
            _ if ctrl => None,
            _ => char::from_u32(self.state.key_get_utf32(keycode))
                .filter(|c| *c != '\0' && !c.is_control())
                .map(Key::Char),
        };
        let repeats = key.is_some() && self.state.get_keymap().key_repeats(keycode);
        (key, repeats)
    }

    /// Caps Lock, if it changed.
    fn caps_changed(&mut self) -> Option<bool> {
        let caps = self
            .state
            .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_LOCKED);
        (caps != self.caps).then(|| {
            self.caps = caps;
            caps
        })
    }
}

struct Repeat {
    code: u32,
    key: Key,
    next: Instant,
}

pub struct Input {
    libinput: Libinput,
    keyboard: Keyboard,
    size: (f32, f32),
    pointer: (f32, f32),
    repeat: Option<Repeat>,
}

impl Input {
    pub fn new(
        link: Rc<RefCell<Link>>,
        layout: Option<&Layout>,
        size: (u32, u32),
    ) -> Result<Input> {
        let keyboard = Keyboard::new(layout)?;
        let mut libinput = Libinput::new_from_path(Opener(link));
        let mut paths: Vec<String> = std::fs::read_dir("/dev/input")
            .context("listing /dev/input")?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with("event"))
            .map(|name| format!("/dev/input/{name}"))
            .collect();
        paths.sort();
        let mut added = 0;
        for path in &paths {
            if libinput.path_add_device(path).is_some() {
                added += 1;
            }
        }
        log(
            WHO,
            format_args!("{added} of {} input devices in use", paths.len()),
        );
        let size = (size.0 as f32, size.1 as f32);
        Ok(Input {
            libinput,
            keyboard,
            size,
            pointer: (size.0 / 2.0, size.1 / 2.0),
            repeat: None,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }

    pub fn set_layout(&mut self, layout: &Layout) {
        self.keyboard.set_layout(layout);
    }

    /// The display left: let go of every device (reclogind has revoked them).
    pub fn suspend(&mut self) {
        self.libinput.suspend();
        self.repeat = None;
    }

    /// The display is back: open the devices again.
    pub fn resume(&mut self) {
        if self.libinput.resume().is_err() {
            log(WHO, "reopening the input devices failed");
        }
    }

    /// When the held key repeats next.
    pub fn repeat_due(&self) -> Option<Instant> {
        self.repeat.as_ref().map(|r| r.next)
    }

    /// The held key's repeat, if it is due.
    pub fn repeat_now(&mut self, now: Instant) -> Option<Happening> {
        let repeat = self.repeat.as_mut()?;
        if now < repeat.next {
            return None;
        }
        repeat.next = now + REPEAT_INTERVAL;
        Some(Happening::Key(repeat.key))
    }

    /// Read what libinput has.
    pub fn read(&mut self) -> Vec<Happening> {
        let mut out = Vec::new();
        if let Err(e) = self.libinput.dispatch() {
            log(WHO, format_args!("reading input: {e}"));
            return out;
        }
        let events: Vec<Event> = self.libinput.by_ref().collect();
        for event in events {
            match event {
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    let code = key.key();
                    let pressed = key.key_state() == KeyState::Pressed;
                    let (meaning, repeats) = self.keyboard.key(code, pressed);
                    if let Some(caps) = self.keyboard.caps_changed() {
                        out.push(Happening::CapsLock(caps));
                    }
                    if pressed {
                        if let Some(meaning) = meaning {
                            out.push(Happening::Key(meaning));
                            self.repeat = repeats.then(|| Repeat {
                                code,
                                key: meaning,
                                next: Instant::now() + REPEAT_DELAY,
                            });
                        }
                    } else if self.repeat.as_ref().is_some_and(|r| r.code == code) {
                        self.repeat = None;
                    }
                }
                Event::Pointer(PointerEvent::Motion(motion)) => {
                    self.pointer.0 =
                        (self.pointer.0 + motion.dx() as f32).clamp(0.0, self.size.0 - 1.0);
                    self.pointer.1 =
                        (self.pointer.1 + motion.dy() as f32).clamp(0.0, self.size.1 - 1.0);
                    out.push(Happening::Pointer(self.pointer.0, self.pointer.1));
                }
                Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
                    self.pointer.0 = motion.absolute_x_transformed(self.size.0 as u32) as f32;
                    self.pointer.1 = motion.absolute_y_transformed(self.size.1 as u32) as f32;
                    out.push(Happening::Pointer(self.pointer.0, self.pointer.1));
                }
                Event::Pointer(PointerEvent::Button(button)) => {
                    if button.button() == BTN_LEFT && button.button_state() == ButtonState::Pressed
                    {
                        out.push(Happening::Press(self.pointer.0, self.pointer.1));
                    }
                }
                Event::Touch(TouchEvent::Down(down)) => {
                    let x = down.x_transformed(self.size.0 as u32) as f32;
                    let y = down.y_transformed(self.size.1 as u32) as f32;
                    out.push(Happening::Press(x, y));
                }
                _ => {}
            }
        }
        out
    }
}
