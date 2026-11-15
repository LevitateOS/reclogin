//! What the person has chosen, typed and focused on the login screen, and
//! how keys and clicks change it. No drawing and no devices here.

use reclogin::protocol::{LoggedIn, MessageKind, PowerAction, PromptKind, State, User, Who};
use zeroize::{Zeroize, Zeroizing};

/// The longest password accepted, in characters. With at most four bytes a
/// character it fits the storage reserved up front, so it is never moved.
const PASSWORD_MAX_CHARS: usize = 256;
const PASSWORD_CAPACITY: usize = PASSWORD_MAX_CHARS * 4;

/// The longest typed user name, in characters.
pub const USER_NAME_MAX_CHARS: usize = 128;

/// Whether `c` may be part of a typed user name: no control characters, no
/// white space, no `/` and no `:` (the daemon refuses the same).
fn name_char(c: char, first: bool) -> bool {
    !c.is_control() && !c.is_whitespace() && c != '/' && c != ':' && !(first && c == '-')
}

/// A user name as it may be sent: only what a login name can be.
pub fn clean_user_name(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        if out.chars().count() >= USER_NAME_MAX_CHARS {
            break;
        }
        if name_char(c, out.is_empty()) {
            out.push(c);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Users,
    UserName,
    Password,
    Session,
    Layout,
    LogIn,
    /// "Cancel" while PAM asks further questions, and on the power question.
    Cancel,
    /// A power action of the footer (Tab reaches them after "Log in").
    Power(PowerAction),
    /// "… anyway" on the power question.
    Confirm,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Do what the screen's main button does: log the chosen person in with
    /// the typed password, start the guest, return to a running session, or
    /// answer PAM's question with what is typed.
    Submit,
    /// Abandon the login attempt (its further questions).
    Cancel,
    SwitchConsole(u32),
    /// The person picked this session (`Session::id`).
    ChooseSession(String),
    /// Switch to the next keyboard layout.
    NextLayout,
    /// Ask for a power action; `confirmed` once the person has been told
    /// who else is logged in and agreed.
    Power {
        action: PowerAction,
        confirmed: bool,
    },
}

/// The row of the list that is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A user of the list, by account name.
    User(String),
    Guest,
    /// "Other user…".
    Other,
}

/// Which screen the form is on while no further PAM questions are asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// A user who is not logged in: password.
    Login,
    /// A user with a running session: the button returns to it.
    Return,
    Guest,
    /// "Other user…": a user name and a password.
    Other,
    /// No account is listed: the same form, with a note in place of the list.
    NoUsers,
}

impl Screen {
    pub fn has_password(self) -> bool {
        matches!(self, Screen::Login | Screen::Other | Screen::NoUsers)
    }

    pub fn has_user_name(self) -> bool {
        matches!(self, Screen::Other | Screen::NoUsers)
    }
}

/// What the main button starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Goal {
    Begin(Who),
    Return(String),
}

/// What PAM said and asked during the login attempt under way.
#[derive(Default)]
pub struct Talk {
    /// Messages (PAM's, and reclogind's own `Success` line), word for word,
    /// in order, since the attempt began. Kept after a failure: they say why.
    pub messages: Vec<(MessageKind, String)>,
    /// Questions after the first password, in order; the last one without
    /// an answer is the one being typed (into `Form::password`).
    pub fields: Vec<Field>,
    /// The question the next Enter answers: its prompt id and kind.
    pub asking: Option<(u64, PromptKind)>,
    /// Whether PAM asked anything yet. Only its first question, when it is a
    /// secret one, takes the password typed on the login screen.
    pub asked: bool,
}

pub struct Field {
    /// PAM's prompt, word for word.
    pub label: String,
    pub answer: Option<Answered>,
}

/// What an answered question still shows. A secret answer keeps only its
/// length: the text itself was sent and overwritten.
pub enum Answered {
    Hidden(usize),
    Shown(String),
}

pub struct Form {
    /// The chosen row of the list.
    pub row: Row,
    pub focus: Focus,
    pub password: Zeroizing<String>,
    pub show_password: bool,
    /// The typed user name ("Other user…"); not a secret.
    pub user_name: String,
    pub caps_lock: bool,
    /// The last message, shown under the password field.
    pub notice: Option<(MessageKind, String)>,
    /// A login attempt is under way.
    pub busy: bool,
    /// "Cancel" was chosen and sent; the form waits, busy, for reclogind's
    /// `auth_result` before anything else can be sent.
    pub cancelling: bool,
    /// `Session::id` of the session to start.
    pub session: Option<String>,
    /// The session menu is open, with this entry (index into
    /// `State::sessions`) highlighted.
    pub menu: Option<usize>,
    pub talk: Talk,
    /// The power question ("Shut down for everyone?") is shown for this
    /// action. reclogind decides when it is asked (`power_question`); the
    /// greeter never opens it by itself.
    pub question: Option<PowerAction>,
    /// Who reclogind said is logged in, for the question.
    pub question_people: Vec<LoggedIn>,
    /// `return_to` was sent: the answer clears `busy` again (success too,
    /// since the screen is not shown any more).
    pub returning: bool,
    /// The on-screen keyboard.
    pub keyboard: crate::osk::Osk,
}

impl Form {
    pub fn new(state: &State) -> Form {
        let mut form = Form {
            row: Row::Other,
            focus: Focus::Password,
            password: Zeroizing::new(String::with_capacity(PASSWORD_CAPACITY)),
            show_password: false,
            user_name: String::new(),
            caps_lock: false,
            notice: None,
            busy: false,
            cancelling: false,
            session: None,
            menu: None,
            talk: Talk::default(),
            question: None,
            question_people: Vec::new(),
            returning: false,
            keyboard: crate::osk::Osk::default(),
        };
        form.row = form.first_row(state);
        form.session = form.default_session(state);
        form.focus = form.entry_focus(state);
        form
    }

    /// The row chosen when nothing was: the last user who logged in, else the
    /// first, else "Other user…".
    fn first_row(&self, state: &State) -> Row {
        state
            .last_user
            .as_ref()
            .and_then(|name| state.users.iter().find(|u| &u.name == name))
            .or(state.users.first())
            .map_or(Row::Other, |u| Row::User(u.name.clone()))
    }

    /// Bring the choices in line with a new `State`: keep the chosen row and
    /// session where they still exist.
    pub fn adopt(&mut self, state: &State) {
        let still = match &self.row {
            Row::User(name) => state.users.iter().any(|u| &u.name == name),
            Row::Guest => state.guest_offered && !state.users.is_empty(),
            Row::Other => true,
        };
        let before = self.row.clone();
        if !still || (state.users.is_empty() && self.row != Row::Other) {
            self.row = self.first_row(state);
        }
        if self.row != before
            || !self
                .session
                .as_ref()
                .is_some_and(|id| state.sessions.iter().any(|s| &s.id == id))
        {
            self.session = self.default_session(state);
        }
        if self.row != before {
            self.focus = self.entry_focus(state);
        }
        // A power action that is gone: its question closes. (Whether it is
        // still needed is reclogind's to say, when it is answered.)
        if self.question.is_some_and(|a| !power_allowed(state, a)) {
            self.question = None;
            self.focus = self.entry_focus(state);
        }
        if let Focus::Power(action) = self.focus {
            if !power_allowed(state, action) {
                self.focus = self.entry_focus(state);
            }
        }
        // The chosen person got a session (or lost it) while the screen was
        // shown: the screen changes (password field, or the return button),
        // and focus must not stay on a field that is gone.
        let screen = self.screen(state);
        if !self.talking()
            && ((self.focus == Focus::Password && !screen.has_password())
                || (self.focus == Focus::UserName && !screen.has_user_name()))
        {
            self.focus = self.entry_focus(state);
        }
        self.menu = None;
    }

    /// The session a person starts unless they pick another: the one they
    /// used last, else the first.
    fn default_session(&self, state: &State) -> Option<String> {
        let user = match &self.row {
            Row::User(name) => Some(name),
            _ => None,
        };
        user.and_then(|name| state.last_session_for.get(name))
            .filter(|id| state.sessions.iter().any(|s| &s.id == *id))
            .cloned()
            .or_else(|| state.sessions.first().map(|s| s.id.clone()))
    }

    /// The chosen user of the list, with the index of its row.
    pub fn chosen<'a>(&self, state: &'a State) -> Option<(usize, &'a User)> {
        match &self.row {
            Row::User(name) => state
                .users
                .iter()
                .enumerate()
                .find(|(_, u)| &u.name == name),
            _ => None,
        }
    }

    pub fn screen(&self, state: &State) -> Screen {
        if state.users.is_empty() {
            return Screen::NoUsers;
        }
        match &self.row {
            Row::Guest => Screen::Guest,
            Row::Other => Screen::Other,
            Row::User(_) => match self.chosen(state) {
                Some((_, user)) if user.logged_in => Screen::Return,
                _ => Screen::Login,
            },
        }
    }

    /// Whether the session picker is shown: only when there is a choice.
    pub fn picker(&self, state: &State) -> bool {
        state.sessions.len() > 1
            && !self.talking()
            && !matches!(self.screen(state), Screen::Return)
            && !(self.screen(state) == Screen::Guest && guest_running(state))
    }

    /// Where focus starts on the screen: where the person types first.
    fn entry_focus(&self, state: &State) -> Focus {
        match self.screen(state) {
            Screen::Login => Focus::Password,
            Screen::Other | Screen::NoUsers => Focus::UserName,
            Screen::Guest | Screen::Return => Focus::LogIn,
        }
    }

    /// A row was chosen by pointer or by Enter: focus goes where typing
    /// starts.
    pub fn entered(&mut self, state: &State) {
        self.focus = self.entry_focus(state);
    }

    /// Whether PAM asks further questions: the screen shows them instead
    /// of the user list and the password.
    pub fn talking(&self) -> bool {
        !self.talk.fields.is_empty()
    }

    /// PAM asks a question the person must answer.
    pub fn ask(&mut self, id: u64, kind: PromptKind, label: String) {
        self.talk.fields.push(Field {
            label,
            answer: None,
        });
        self.talk.asking = Some((id, kind));
        self.busy = false;
        self.focus = Focus::Password;
        self.show_password = false;
        self.notice = None;
        self.menu = None;
        self.clear_password();
    }

    /// The question being typed was answered: keep what it may still show,
    /// then clear the field.
    pub fn answered(&mut self) {
        let kind = self.talk.asking.take().map(|(_, kind)| kind);
        if let Some(field) = self
            .talk
            .fields
            .iter_mut()
            .rev()
            .find(|f| f.answer.is_none())
        {
            field.answer = Some(match kind {
                Some(PromptKind::Visible) => Answered::Shown(self.password.to_string()),
                _ => Answered::Hidden(self.password.chars().count()),
            });
        }
        self.clear_password();
        self.show_password = false;
        self.notice = None;
        self.busy = true;
    }

    /// The login attempt ended. On failure the screen comes back with the
    /// user (or typed name) and session kept, the password cleared, focus on
    /// the password (the button where there is none), `text` (if any) as its
    /// error line and PAM's messages above it. On success the form stays busy
    /// until reclogind lets the greeter go.
    pub fn finish(&mut self, ok: bool, text: String, state: &State) {
        self.busy = ok;
        self.talk.fields.clear();
        self.talk.asking = None;
        self.talk.asked = false;
        self.clear_password();
        self.show_password = false;
        if !ok {
            self.focus = if self.screen(state).has_password() {
                Focus::Password
            } else {
                Focus::LogIn
            };
            self.notice = (!text.is_empty()).then_some((MessageKind::Error, text));
        }
    }

    /// Choose a row of the list. Nothing typed for the other row is kept
    /// except the typed user name.
    pub fn select(&mut self, row: Row, state: &State) {
        if self.busy || self.talking() {
            return;
        }
        match &row {
            Row::User(name) if !state.users.iter().any(|u| &u.name == name) => return,
            Row::Guest if !state.guest_offered => return,
            _ => {}
        }
        if self.row != row {
            self.row = row;
            self.clear_password();
            self.show_password = false;
            self.notice = None;
            self.talk.messages.clear();
            self.session = self.default_session(state);
        }
    }

    /// The rows of the list, in order.
    pub fn rows(state: &State) -> Vec<Row> {
        let mut rows: Vec<Row> = state
            .users
            .iter()
            .map(|u| Row::User(u.name.clone()))
            .collect();
        if state.guest_offered {
            rows.push(Row::Guest);
        }
        rows.push(Row::Other);
        rows
    }

    pub fn clear_password(&mut self) {
        // Zeroize overwrites the whole storage; keep the reserved capacity.
        self.password.zeroize();
        if self.password.capacity() < PASSWORD_CAPACITY {
            self.password.reserve_exact(PASSWORD_CAPACITY);
        }
    }

    fn focus_order(&self, state: &State) -> Vec<Focus> {
        let multiple_layouts = state.layouts.len() > 1;
        if self.question.is_some() {
            let mut order = vec![Focus::Confirm, Focus::Cancel];
            if multiple_layouts {
                order.push(Focus::Layout);
            }
            order.extend(power_focuses(state));
            return order;
        }
        if self.talking() {
            let mut order = vec![Focus::Password, Focus::LogIn, Focus::Cancel];
            if multiple_layouts {
                order.push(Focus::Layout);
            }
            order.extend(power_focuses(state));
            return order;
        }
        let screen = self.screen(state);
        let mut order = Vec::new();
        if screen != Screen::NoUsers {
            order.push(Focus::Users);
        }
        if screen.has_user_name() {
            order.push(Focus::UserName);
        }
        if screen.has_password() {
            order.push(Focus::Password);
        }
        if self.picker(state) {
            order.push(Focus::Session);
        }
        if multiple_layouts {
            order.push(Focus::Layout);
        }
        order.push(Focus::LogIn);
        order.extend(power_focuses(state));
        order
    }

    fn move_focus(&mut self, state: &State, forward: bool) {
        let order = self.focus_order(state);
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        self.focus = order[next];
    }

    /// Which field typed characters go to, for a screen: the user name until
    /// there is one (or while it is focused), else the password.
    fn typing_target(&self, state: &State) -> Option<Focus> {
        let screen = self.screen(state);
        if self.talking() {
            return Some(Focus::Password);
        }
        match (screen.has_user_name(), screen.has_password()) {
            (true, _) if self.focus == Focus::UserName => Some(Focus::UserName),
            (true, _) if self.focus != Focus::Password && self.user_name.is_empty() => {
                Some(Focus::UserName)
            }
            (_, true) => Some(Focus::Password),
            _ => None,
        }
    }

    /// Open the session menu on the session in use.
    pub fn open_menu(&mut self, state: &State) {
        if self.busy || !self.picker(state) {
            return;
        }
        let at = self
            .session
            .as_ref()
            .and_then(|id| state.sessions.iter().position(|s| &s.id == id))
            .unwrap_or(0);
        self.menu = Some(at);
        self.focus = Focus::Session;
    }

    /// Choose the session at `index` of the menu and close it.
    pub fn pick_session(&mut self, index: usize, state: &State) -> Option<Action> {
        self.menu = None;
        self.focus = Focus::Session;
        let session = state.sessions.get(index)?;
        self.session = Some(session.id.clone());
        Some(Action::ChooseSession(session.id.clone()))
    }

    fn menu_key(&mut self, key: Key, state: &State) -> Option<Action> {
        let at = self.menu?;
        let last = state.sessions.len().saturating_sub(1);
        match key {
            Key::Console(n) => return Some(Action::SwitchConsole(n)),
            Key::Up => self.menu = Some(at.saturating_sub(1)),
            Key::Down => self.menu = Some((at + 1).min(last)),
            Key::Enter | Key::Char(' ') => return self.pick_session(at, state),
            Key::Escape => self.menu = None,
            Key::Tab | Key::BackTab => {
                self.menu = None;
                self.move_focus(state, key == Key::Tab);
            }
            _ => {}
        }
        None
    }

    /// A footer power action was chosen (pointer, or Enter or Space on it):
    /// asked for at once, unconfirmed. When people are logged in reclogind
    /// answers a restart or shut down with its question ([`Form::ask_power`]).
    /// Not while a login is under way.
    pub fn press_power(&mut self, action: PowerAction, state: &State) -> Option<Action> {
        if self.busy || !power_allowed(state, action) {
            return None;
        }
        self.notice = None;
        self.menu = None;
        self.question = None;
        self.focus = Focus::Power(action);
        Some(Action::Power {
            action,
            confirmed: false,
        })
    }

    /// reclogind asks before `action`: `people` are logged in.
    pub fn ask_power(&mut self, action: PowerAction, people: Vec<LoggedIn>, state: &State) {
        if self.busy || !power_allowed(state, action) {
            return;
        }
        self.menu = None;
        self.notice = None;
        self.question = Some(action);
        self.question_people = people;
        // Not on "… anyway": Enter must not shut everyone's work down.
        self.focus = Focus::Cancel;
    }

    /// Leave the power question: focus goes back to the footer action it
    /// was opened from.
    pub fn close_question(&mut self, state: &State) {
        let Some(action) = self.question.take() else {
            return;
        };
        self.focus = if power_allowed(state, action) {
            Focus::Power(action)
        } else {
            self.entry_focus(state)
        };
    }

    fn question_key(&mut self, key: Key, state: &State) -> Option<Action> {
        let action = self.question?;
        match key {
            Key::Console(n) => return Some(Action::SwitchConsole(n)),
            Key::Tab => self.move_focus(state, true),
            Key::BackTab => self.move_focus(state, false),
            Key::Escape => self.close_question(state),
            Key::Enter | Key::Char(' ') => match self.focus {
                Focus::Confirm => {
                    return Some(Action::Power {
                        action,
                        confirmed: true,
                    })
                }
                Focus::Cancel => self.close_question(state),
                Focus::Power(other) => return self.press_power(other, state),
                Focus::Layout => return (state.layouts.len() > 1).then_some(Action::NextLayout),
                _ => {}
            },
            _ => {}
        }
        None
    }

    /// Apply a key. Returns what the program must do beyond redrawing.
    pub fn key(&mut self, key: Key, state: &State) -> Option<Action> {
        if self.question.is_some() {
            return self.question_key(key, state);
        }
        if self.menu.is_some() {
            return self.menu_key(key, state);
        }
        match key {
            Key::Console(n) => return Some(Action::SwitchConsole(n)),
            Key::Tab => self.move_focus(state, true),
            Key::BackTab => self.move_focus(state, false),
            Key::Up | Key::Down if self.focus == Focus::Users && !self.talking() => {
                let rows = Form::rows(state);
                if let Some(at) = rows.iter().position(|r| *r == self.row) {
                    let next = if key == Key::Up {
                        at.checked_sub(1)
                    } else {
                        Some(at + 1)
                    };
                    if let Some(row) = next.and_then(|i| rows.get(i)) {
                        // Browsing the list: focus stays on it.
                        self.select(row.clone(), state);
                    }
                }
            }
            Key::Enter => match self.focus {
                Focus::Users => self.entered(state),
                Focus::UserName => self.focus = Focus::Password,
                Focus::Password | Focus::LogIn => return self.submit(state),
                Focus::Cancel => return Some(Action::Cancel),
                Focus::Session => self.open_menu(state),
                Focus::Layout => return (state.layouts.len() > 1).then_some(Action::NextLayout),
                Focus::Power(action) => return self.press_power(action, state),
                Focus::Confirm => {}
            },
            Key::Char(' ')
                if matches!(
                    self.focus,
                    Focus::Session | Focus::LogIn | Focus::Cancel | Focus::Layout | Focus::Power(_)
                ) =>
            {
                match self.focus {
                    Focus::Power(action) => return self.press_power(action, state),
                    Focus::Session => self.open_menu(state),
                    Focus::Layout => {
                        return (state.layouts.len() > 1).then_some(Action::NextLayout)
                    }
                    Focus::Cancel => return Some(Action::Cancel),
                    _ => return self.submit(state),
                }
            }
            Key::Escape if !self.busy => match self.focus {
                Focus::Password => self.clear_password(),
                Focus::UserName => self.user_name.clear(),
                _ => {}
            },
            Key::Backspace if !self.busy => match self.typing_target(state) {
                Some(Focus::UserName) => {
                    self.focus = Focus::UserName;
                    self.user_name.pop();
                }
                Some(_) => {
                    self.focus = Focus::Password;
                    self.password.pop();
                }
                None => {}
            },
            Key::Char(c) if !self.busy && !c.is_control() => match self.typing_target(state) {
                Some(Focus::UserName) => {
                    self.focus = Focus::UserName;
                    if self.user_name.chars().count() < USER_NAME_MAX_CHARS
                        && name_char(c, self.user_name.is_empty())
                    {
                        self.user_name.push(c);
                    }
                }
                Some(_) => {
                    self.focus = Focus::Password;
                    if self.password.chars().count() < PASSWORD_MAX_CHARS {
                        self.password.push(c);
                    }
                }
                None => {}
            },
            _ => {}
        }
        None
    }

    /// What the main button does now, if anything.
    pub fn goal(&self, state: &State) -> Option<Goal> {
        match self.screen(state) {
            Screen::Login => self.chosen(state).map(|(_, u)| {
                Goal::Begin(Who::User {
                    name: u.name.clone(),
                })
            }),
            Screen::Return => self
                .chosen(state)
                .map(|(_, u)| Goal::Return(u.name.clone())),
            // A guest session already runs: back to it, not a second one.
            Screen::Guest if guest_running(state) => Some(Goal::Return(GUEST.into())),
            Screen::Guest => Some(Goal::Begin(Who::Guest)),
            Screen::Other | Screen::NoUsers => {
                let name = clean_user_name(&self.user_name);
                (!name.is_empty()).then_some(Goal::Begin(Who::TypedUser { name }))
            }
        }
    }

    fn submit(&mut self, state: &State) -> Option<Action> {
        if self.busy {
            return None;
        }
        let screen = self.screen(state);
        if self.talk.asking.is_none() {
            if screen.has_user_name() && clean_user_name(&self.user_name).is_empty() {
                self.notice = Some((MessageKind::Error, "Type a user name first.".into()));
                self.focus = Focus::UserName;
                return None;
            }
            if !screen.has_password() {
                return self.goal(state).map(|_| Action::Submit);
            }
        }
        // An empty secret never reaches PAM; an empty visible answer may.
        let secret = match self.talk.asking {
            Some((_, kind)) => kind == PromptKind::Secret,
            None => true,
        };
        if secret && self.password.is_empty() {
            self.notice = Some((MessageKind::Error, "Type your password first.".into()));
            self.focus = Focus::Password;
            return None;
        }
        Some(Action::Submit)
    }
}

/// The guest's account name, as the login service reports its session.
const GUEST: &str = "guest";

/// Whether a guest session is running (the guest is not in `State::users`;
/// the login service's list of running sessions has it).
pub fn guest_running(state: &State) -> bool {
    state.running.iter().any(|s| s.user == GUEST)
}

/// Whether the footer offers `action` (the login service allows it).
pub fn power_allowed(state: &State, action: PowerAction) -> bool {
    match action {
        PowerAction::Sleep => state.power.sleep,
        PowerAction::Restart => state.power.restart,
        PowerAction::ShutDown => state.power.shut_down,
    }
}

/// The footer's power actions in the order drawn and tabbed.
fn power_focuses(state: &State) -> Vec<Focus> {
    [
        PowerAction::Sleep,
        PowerAction::Restart,
        PowerAction::ShutDown,
    ]
    .into_iter()
    .filter(|a| power_allowed(state, *a))
    .map(Focus::Power)
    .collect()
}
