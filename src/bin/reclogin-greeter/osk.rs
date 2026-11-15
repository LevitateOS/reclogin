//! The on-screen keyboard: what its keys give and what pressing them does.
//! No drawing ([`crate::screens::keyboard`]) and no devices here.
//!
//! The letters come from the current keyboard layout. The layout's keymap
//! (the one the greeter already compiled for physical keys) is asked which
//! character each key position gives at each level; the keyboard has the
//! four rows of the design at the evdev positions of a PC keyboard: the
//! digit row (`1`..`0`), `Q`..`P`, `A`..`L` and `Z`..`.`. So a German layout
//! shows z where the US one has y.
//!
//! The symbols key switches the three letter rows to the layout's third
//! level (AltGr; the fourth with Shift) when that level is a symbol layer:
//! at least 14 of the 28 keys have a character there that is not a letter,
//! and three quarters of the characters that are there are such. Layouts
//! whose AltGr gives letters as well (German's æ, ø, þ, ł next to `@` and
//! `€`) or nothing (US) get a fixed table of common
//! symbols instead ([`FIXED`], with a second page on Shift). The digit row
//! never changes with the symbols key.

use reclogin::protocol::State;
use xkbcommon::xkb;

use crate::form::{Focus, Form, Key};

/// Keys in each of the four rows: digits, `Q` row, `A` row, `Z` row.
pub const ROW_LEN: [usize; 4] = [10, 10, 9, 9];

/// The evdev code of the first key of each row (the keys follow in order,
/// except that the `A` row's follow `A`..`L` and the `Z` row's `Z`..`.`).
const ROW_START: [u32; 4] = [2, 16, 30, 44];

/// One key of the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyId {
    /// A character key: its row (0 digits, 1..=3 letters) and place in it.
    Char(u8, u8),
    Enter,
    Shift,
    Delete,
    /// "!#1" / "abc": the letter rows switch to symbols and back.
    Symbols,
    Layout,
    Space,
    Hide,
}

/// The characters one key gives: plain, Shift, third level, third level
/// with Shift.
#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    levels: [Option<char>; 4],
}

/// What the keys of a layout give.
#[derive(Debug, Clone)]
pub struct Table {
    rows: [Vec<Cell>; 4],
    /// The layout's third level is a symbol layer on most of the letter keys.
    third_level: bool,
}

/// A symbols table for layouts with no third level: page one, and page two
/// (Shift) in the three letter rows.
const FIXED: [[&str; 3]; 2] = [
    ["!@#$%^&*()", "-_=+[]{};", ":'\"\\|/<>?"],
    ["~`,.€£¥°§±", "©®µ¶¿¡«»·", "²³¼½¾×÷¢¤"],
];

/// The US letters, for a layout that does not compile.
const US: [[&str; 2]; 4] = [
    ["1234567890", "!@#$%^&*()"],
    ["qwertyuiop", "QWERTYUIOP"],
    ["asdfghjkl", "ASDFGHJKL"],
    ["zxcvbnm,.", "ZXCVBNM<>"],
];

impl Table {
    /// The US keys, for when the keymap cannot be read.
    pub fn plain_us() -> Table {
        let rows = std::array::from_fn(|r| {
            US[r][0]
                .chars()
                .zip(US[r][1].chars())
                .map(|(a, b)| Cell {
                    levels: [Some(a), Some(b), None, None],
                })
                .collect()
        });
        Table {
            rows,
            third_level: false,
        }
    }

    /// Ask `keymap` (layout group 0) what each key position gives.
    pub fn from_keymap(keymap: &xkb::Keymap) -> Table {
        let level = |code: u32, level: u32| -> Option<char> {
            let key = xkb::Keycode::new(code + 8);
            if level >= keymap.num_levels_for_key(key, 0) {
                return None;
            }
            let sym = *keymap.key_get_syms_by_level(key, 0, level).first()?;
            let text = xkb::keysym_to_utf8(sym);
            let mut chars = text.chars();
            let c = chars.next()?;
            (chars.next().is_none() && !c.is_control() && c != '\0').then_some(c)
        };
        let (mut third, mut symbols) = (0, 0);
        let rows: [Vec<Cell>; 4] = std::array::from_fn(|r| {
            (0..ROW_LEN[r])
                .map(|i| {
                    let code = ROW_START[r] + i as u32;
                    let mut cell = Cell {
                        levels: [
                            level(code, 0),
                            level(code, 1),
                            level(code, 2),
                            level(code, 3),
                        ],
                    };
                    if r == 0 {
                        // The digit row types digits: on a layout that has
                        // them under Shift (French), they still show plain.
                        let digit = char::from_digit((i as u32 + 1) % 10, 10);
                        let is_digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
                        if !is_digit(cell.levels[0]) && is_digit(cell.levels[1]) {
                            cell.levels = [
                                cell.levels[1],
                                cell.levels[0],
                                cell.levels[2],
                                cell.levels[3],
                            ];
                        } else if cell.levels[0].is_none() {
                            cell.levels[0] = digit;
                        }
                    } else if let Some(c) = cell.levels[2] {
                        third += 1;
                        symbols += usize::from(!c.is_alphabetic());
                    }
                    cell
                })
                .collect()
        });
        Table {
            rows,
            third_level: symbols >= 14 && symbols * 4 >= third * 3,
        }
    }

    /// What a character key shows and types now.
    pub fn glyph(&self, row: usize, index: usize, shift: bool, symbols: bool) -> Option<char> {
        if row > 0 && symbols {
            if self.third_level {
                let cell = self.rows.get(row)?.get(index)?;
                return if shift {
                    cell.levels[3].or(cell.levels[2])
                } else {
                    cell.levels[2]
                };
            }
            return FIXED[usize::from(shift)][row - 1].chars().nth(index);
        }
        let cell = self.rows.get(row)?.get(index)?;
        if shift {
            cell.levels[1].or_else(|| cell.levels[0].and_then(|c| c.to_uppercase().next()))
        } else {
            cell.levels[0]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shift {
    #[default]
    Off,
    /// For the next character only.
    Once,
    /// Pressed twice: stays until pressed again.
    Lock,
}

/// What pressing a key asks of the greeter beyond [`Osk`]'s own state.
pub enum Effect {
    None,
    /// Give the form this key, as if it had been typed.
    Type(Key),
    NextLayout,
}

/// The keyboard's state; part of the form.
pub struct Osk {
    pub open: bool,
    pub shift: Shift,
    pub symbols: bool,
    /// The key held down by the pointer or a finger, drawn pressed.
    pub pressed: Option<KeyId>,
    /// The pointer or finger is up but the key has not been drawn pressed
    /// yet: it is cleared by the next [`Osk::drawn`].
    release_pending: bool,
    pub table: Table,
    /// `Input::layout_serial` the table was read at.
    pub table_serial: Option<u32>,
}

impl Default for Osk {
    fn default() -> Osk {
        Osk {
            open: false,
            shift: Shift::Off,
            symbols: false,
            pressed: None,
            release_pending: false,
            table: Table::plain_us(),
            table_serial: None,
        }
    }
}

impl Osk {
    /// The glyph of a character key as drawn now.
    pub fn glyph(&self, row: usize, index: usize) -> Option<char> {
        self.table
            .glyph(row, index, self.shift != Shift::Off, self.symbols)
    }

    /// A key was pressed (pointer down or touch). Returns what the greeter
    /// must do.
    pub fn press(&mut self, id: KeyId, can_switch_layout: bool) -> Effect {
        self.pressed = Some(id);
        self.release_pending = false;
        match id {
            KeyId::Char(row, index) => match self.glyph(row.into(), index.into()) {
                Some(c) => {
                    if self.shift == Shift::Once {
                        self.shift = Shift::Off;
                    }
                    Effect::Type(Key::Char(c))
                }
                None => Effect::None,
            },
            KeyId::Space => {
                if self.shift == Shift::Once {
                    self.shift = Shift::Off;
                }
                Effect::Type(Key::Char(' '))
            }
            KeyId::Enter => Effect::Type(Key::Enter),
            KeyId::Delete => Effect::Type(Key::Backspace),
            KeyId::Shift => {
                self.shift = match self.shift {
                    Shift::Off => Shift::Once,
                    Shift::Once => Shift::Lock,
                    Shift::Lock => Shift::Off,
                };
                Effect::None
            }
            KeyId::Symbols => {
                self.symbols = !self.symbols;
                Effect::None
            }
            KeyId::Layout => {
                if can_switch_layout {
                    Effect::NextLayout
                } else {
                    Effect::None
                }
            }
            KeyId::Hide => {
                self.open = false;
                self.reset();
                Effect::None
            }
        }
    }

    /// The pointer or finger came up.
    pub fn release(&mut self) {
        if self.pressed.is_some() {
            self.release_pending = true;
        }
    }

    /// A frame was drawn: a key released before it showed pressed is let go.
    pub fn drawn(&mut self) -> bool {
        let changed = self.release_pending;
        if changed {
            self.release_pending = false;
            self.pressed = None;
        }
        changed
    }

    /// Back to lower case and letters.
    pub fn reset(&mut self) {
        self.shift = Shift::Off;
        self.symbols = false;
        self.pressed = None;
        self.release_pending = false;
    }
}

/// Whether the screen the form is on has a field to type into (so the
/// keyboard can be shown): a login password, a user name, or a question.
pub fn eligible(form: &Form, state: &State) -> bool {
    form.question.is_none() && (form.talking() || form.screen(state).has_password())
}

/// Whether the keyboard is on the screen now.
pub fn visible(form: &Form, state: &State) -> bool {
    form.keyboard.open && eligible(form, state)
}

/// Open the keyboard (the footer's button, or a touch on a field). Focus
/// goes to a field if it is not on one, so keys type somewhere.
pub fn open(form: &mut Form, state: &State) {
    if !eligible(form, state) {
        return;
    }
    form.menu = None;
    form.keyboard.open = true;
    form.keyboard.reset();
    if form.talking() {
        form.focus = Focus::Password;
    } else if !matches!(form.focus, Focus::UserName | Focus::Password) {
        form.entered(state);
    }
}
