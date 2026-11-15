//! What the person has chosen, typed and focused on the login screen, and
//! how keys and clicks change it. No drawing and no devices here.

use reclogin::protocol::{MessageKind, State};
use zeroize::{Zeroize, Zeroizing};

/// The longest password accepted, in characters. With at most four bytes a
/// character it fits the storage reserved up front, so it is never moved.
const PASSWORD_MAX_CHARS: usize = 256;
const PASSWORD_CAPACITY: usize = PASSWORD_MAX_CHARS * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Users,
    Password,
    Session,
    Layout,
    LogIn,
}

/// A key, as the keyboard layout produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Tab,
    BackTab,
    Up,
    Down,
    Escape,
    /// Ctrl+Alt+F1 to F12.
    Console(u32),
}

/// What a key or click asks the program to do beyond changing the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Log the chosen user in with the typed password.
    Submit,
    SwitchConsole(u32),
}

pub struct Form {
    /// Index into `State::users`.
    pub chosen: Option<usize>,
    pub focus: Focus,
    pub password: Zeroizing<String>,
    pub show_password: bool,
    pub caps_lock: bool,
    /// The last message, shown under the password field.
    pub notice: Option<(MessageKind, String)>,
    /// A login attempt is under way.
    pub busy: bool,
    /// `Session::id` of the session to start.
    pub session: Option<String>,
}

impl Form {
    pub fn new(state: &State) -> Form {
        let mut form = Form {
            chosen: None,
            focus: Focus::Password,
            password: Zeroizing::new(String::with_capacity(PASSWORD_CAPACITY)),
            show_password: false,
            caps_lock: false,
            notice: None,
            busy: false,
            session: None,
        };
        form.adopt(state);
        form
    }

    /// Bring the choices in line with a new `State`: keep the chosen user
    /// and session where they still exist.
    pub fn adopt(&mut self, state: &State) {
        let still = self
            .chosen
            .and_then(|i| state.users.get(i))
            .map(|u| u.name.clone())
            .and_then(|name| state.users.iter().position(|u| u.name == name));
        let preferred = state
            .last_user
            .as_ref()
            .and_then(|name| state.users.iter().position(|u| &u.name == name));
        let chosen = still.or(preferred).or(if state.users.is_empty() {
            None
        } else {
            Some(0)
        });
        let changed = chosen != self.chosen;
        self.chosen = chosen;
        if changed
            || !self
                .session
                .as_ref()
                .is_some_and(|id| state.sessions.iter().any(|s| &s.id == id))
        {
            self.session = self.default_session(state);
        }
    }

    fn default_session(&self, state: &State) -> Option<String> {
        let user = self.chosen.and_then(|i| state.users.get(i));
        user.and_then(|u| state.last_session_for.get(&u.name))
            .filter(|id| state.sessions.iter().any(|s| &s.id == *id))
            .cloned()
            .or_else(|| state.sessions.first().map(|s| s.id.clone()))
    }

    pub fn choose(&mut self, index: usize, state: &State) {
        if index >= state.users.len() || self.busy {
            return;
        }
        if self.chosen != Some(index) {
            self.chosen = Some(index);
            self.clear_password();
            self.notice = None;
            self.session = self.default_session(state);
        }
    }

    pub fn clear_password(&mut self) {
        // Zeroize overwrites the whole storage; keep the reserved capacity.
        self.password.zeroize();
        if self.password.capacity() < PASSWORD_CAPACITY {
            self.password.reserve_exact(PASSWORD_CAPACITY);
        }
    }

    fn focus_order(state: &State) -> Vec<Focus> {
        let mut order = vec![Focus::Users, Focus::Password];
        if !state.sessions.is_empty() {
            order.push(Focus::Session);
        }
        order.push(Focus::Layout);
        order.push(Focus::LogIn);
        order
    }

    fn move_focus(&mut self, state: &State, forward: bool) {
        let order = Form::focus_order(state);
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        self.focus = order[next];
    }

    /// Apply a key. Returns what the program must do beyond redrawing.
    pub fn key(&mut self, key: Key, state: &State) -> Option<Action> {
        match key {
            Key::Console(n) => return Some(Action::SwitchConsole(n)),
            Key::Tab => self.move_focus(state, true),
            Key::BackTab => self.move_focus(state, false),
            Key::Up | Key::Down if self.focus == Focus::Users => {
                if let Some(i) = self.chosen {
                    let next = if key == Key::Up {
                        i.checked_sub(1)
                    } else {
                        Some(i + 1)
                    };
                    if let Some(next) = next {
                        self.choose(next, state);
                    }
                }
            }
            Key::Enter => match self.focus {
                Focus::Users => self.focus = Focus::Password,
                Focus::Password | Focus::LogIn => return self.submit(),
                // The session menu and the layout switcher are later stages.
                Focus::Session | Focus::Layout => {}
            },
            Key::Escape if self.focus == Focus::Password && !self.busy => self.clear_password(),
            Key::Backspace if !self.busy => {
                self.focus = Focus::Password;
                self.password.pop();
            }
            Key::Char(c) if !self.busy && !c.is_control() => {
                self.focus = Focus::Password;
                if self.password.chars().count() < PASSWORD_MAX_CHARS {
                    self.password.push(c);
                }
            }
            _ => {}
        }
        None
    }

    fn submit(&mut self) -> Option<Action> {
        if self.busy || self.chosen.is_none() {
            return None;
        }
        if self.password.is_empty() {
            self.notice = Some((MessageKind::Error, "Type your password first.".into()));
            self.focus = Focus::Password;
            return None;
        }
        Some(Action::Submit)
    }
}
