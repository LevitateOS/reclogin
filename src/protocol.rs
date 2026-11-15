//! The messages between `reclogind` and `reclogin-greeter`.
//!
//! One JSON object per message, ending in a newline, one message per packet
//! of a Unix `SOCK_SEQPACKET` socket pair (see [`crate::wire`]). Every object
//! has a `"type"` field naming the message; the other fields are the
//! message's own. A message that carries file handles says so through
//! [`Framed::handles`]; the handles travel with `SCM_RIGHTS` in the same
//! packet, and a packet whose handle count differs from what its message
//! announces is refused.
//!
//! No message carries a secret. [`ToDaemon::Answer`] announces one: the
//! packet right after it is the answer's raw bytes, sent and received by
//! [`crate::wire::Channel::send_secret`] and
//! [`crate::wire::Channel::recv_secret`] straight from and into a
//! [`Secret`] (overwritten when dropped, never shown by `Debug`). Inside JSON
//! it would pass through serde's intermediate buffer for internally tagged
//! messages, which is freed without being overwritten.
//!
//! Adding a field later: give it `#[serde(default)]` so that an older peer's
//! messages still read. Changing a field's meaning needs a new [`VERSION`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The protocol version both sides must agree on. `Hello` carries it.
pub const VERSION: u32 = 2;

/// The greeter's end of the socket is this file descriptor in the greeter.
pub const GREETER_SOCKET_FD: i32 = 3;

/// The environment variable that names [`GREETER_SOCKET_FD`] to the greeter.
pub const GREETER_SOCKET_ENV: &str = "RECLOGIN_SOCKET";

/// How many file handles a message carries beside it.
pub trait Framed {
    fn handles(&self) -> usize;
    /// The message's name, for logs. Never the content.
    fn kind(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// reclogind to the greeter

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToGreeter {
    /// First message. The greeter checks the version and reads its brand
    /// data from `brand_dir`.
    Hello { version: u32, brand_dir: PathBuf },
    /// The display device, already opened by reclogind, which is its master
    /// while the login console is shown. Carries one handle. `path` is for
    /// logs only.
    Display { path: String },
    /// An open input device, the answer to `OpenDevice` with the same
    /// `request_id`. Carries one handle (read-only, non-blocking).
    Device { request_id: u64 },
    /// `OpenDevice` with this `request_id` was refused.
    DeviceRefused { request_id: u64, reason: String },
    /// Everything the screen shows that comes from the system. Sent at start
    /// and again whenever it changes; the newest one replaces the others.
    State(State),
    /// A question from PAM. The greeter shows `text` word for word and
    /// answers with `Answer { prompt_id: id, .. }`.
    Prompt {
        id: u64,
        kind: PromptKind,
        text: String,
    },
    /// A message from PAM or from reclogind, shown word for word, in order.
    Message { kind: MessageKind, text: String },
    /// The end of a login attempt. `ok: false` returns to the login screen
    /// with `text` (if not empty) as the error line.
    AuthResult { ok: bool, text: String },
    /// The login console is no longer shown: stop drawing and reading input.
    /// The display and the input devices stop working for the greeter.
    Pause,
    /// The login console is shown again: set the mode, redraw, read input.
    Resume,
    /// Exit now.
    Quit,
    /// Answer with `Pong { seq }` at once. Sent every few seconds while the
    /// login console is shown; a greeter that has not answered within ten
    /// seconds is taken as hung, ended and counted as a failure.
    Ping { seq: u64 },
}

impl Framed for ToGreeter {
    fn handles(&self) -> usize {
        match self {
            ToGreeter::Display { .. } | ToGreeter::Device { .. } => 1,
            _ => 0,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            ToGreeter::Hello { .. } => "hello",
            ToGreeter::Display { .. } => "display",
            ToGreeter::Device { .. } => "device",
            ToGreeter::DeviceRefused { .. } => "device_refused",
            ToGreeter::State(_) => "state",
            ToGreeter::Prompt { .. } => "prompt",
            ToGreeter::Message { .. } => "message",
            ToGreeter::AuthResult { .. } => "auth_result",
            ToGreeter::Pause => "pause",
            ToGreeter::Resume => "resume",
            ToGreeter::Quit => "quit",
            ToGreeter::Ping { .. } => "ping",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Typed text is hidden (a password).
    Secret,
    /// Typed text is shown (a user name, a one-time code shown on purpose).
    Visible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Info,
    Error,
    /// A step reclogind confirms in its own words, never PAM's: "Current
    /// password accepted." before an expired password is changed.
    Success,
}

/// What the login screen shows that comes from the system.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub hostname: String,
    /// The users that may be chosen from the list, in the order shown.
    pub users: Vec<User>,
    /// The desktop sessions installed and valid, in the order shown.
    pub sessions: Vec<Session>,
    /// The session each user used last: user name to `Session::id`.
    #[serde(default)]
    pub last_session_for: BTreeMap<String, String>,
    /// The user to preselect; `None` preselects the first in the list.
    #[serde(default)]
    pub last_user: Option<String>,
    /// The keyboard layouts the system is set to, the first is the default.
    pub layouts: Vec<Layout>,
    /// `Layout::name` of the layout in use.
    pub current_layout: String,
    pub power: Power,
    /// Whether the guest entry is shown.
    pub guest_offered: bool,
    /// User names with a running session (for the power question).
    #[serde(default)]
    pub others_logged_in: Vec<String>,
    /// Clock style from the system's language and region.
    #[serde(default = "default_true")]
    pub clock_24h: bool,
    /// Switch the display off after this many seconds without input.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// The account name, what `Begin` names.
    pub name: String,
    /// The name shown; the account name when the account has none.
    pub real_name: String,
    #[serde(default)]
    pub logged_in: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The session file's name without `.desktop`.
    pub id: String,
    /// The name shown (`Name=` of the session file).
    pub name: String,
    pub kind: SessionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Wayland,
    X11,
}

/// One keyboard layout, as libxkbcommon compiles it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// What `SetLayout` and `State::current_layout` name: `layout` or
    /// `layout(variant)`.
    pub name: String,
    /// The name shown, e.g. "English (US)".
    pub label: String,
    /// XKB rules names.
    pub xkb_layout: String,
    #[serde(default)]
    pub xkb_variant: String,
    #[serde(default)]
    pub xkb_model: String,
    #[serde(default)]
    pub xkb_options: String,
}

/// Which power actions logind allows now. Hibernate is never offered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Power {
    pub sleep: bool,
    pub restart: bool,
    pub shut_down: bool,
}

// ---------------------------------------------------------------------------
// the greeter to reclogind

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToDaemon {
    /// The greeter has the display, has drawn, and takes input.
    Ready,
    /// Open an input device for libinput. Only `/dev/input/event*`
    /// character devices are opened, read-only and non-blocking.
    OpenDevice { request_id: u64, path: PathBuf },
    /// libinput closed the device opened for `request_id`.
    CloseDevice { request_id: u64 },
    /// Start a login for this person. PAM's questions come back as `Prompt`.
    Begin { who: Who },
    /// The answer to `Prompt { id: prompt_id, .. }`. The next packet is the
    /// answer itself, raw (`Channel::send_secret`), never JSON.
    Answer { prompt_id: u64 },
    /// Abandon the login attempt in progress.
    Cancel,
    /// The session to start after a successful login.
    ChooseSession { id: String },
    /// Bring this logged-in user's session to the front.
    ReturnTo { user: String },
    /// A power action. `confirmed` is true when the person agreed after
    /// being told that others are logged in.
    Power {
        action: PowerAction,
        confirmed: bool,
    },
    /// Use this layout (`Layout::name`) from now on.
    SetLayout { name: String },
    /// Show this console (Ctrl+Alt+F1 to F12). The greeter has no right to
    /// switch consoles itself, and the kernel does not see keys while the
    /// login console is in graphics mode.
    SwitchConsole { number: u32 },
    /// The answer to `Ping { seq }`.
    Pong { seq: u64 },
}

impl Framed for ToDaemon {
    fn handles(&self) -> usize {
        0
    }

    fn kind(&self) -> &'static str {
        match self {
            ToDaemon::Ready => "ready",
            ToDaemon::OpenDevice { .. } => "open_device",
            ToDaemon::CloseDevice { .. } => "close_device",
            ToDaemon::Begin { .. } => "begin",
            ToDaemon::Answer { .. } => "answer",
            ToDaemon::Cancel => "cancel",
            ToDaemon::ChooseSession { .. } => "choose_session",
            ToDaemon::ReturnTo { .. } => "return_to",
            ToDaemon::Power { .. } => "power",
            ToDaemon::SetLayout { .. } => "set_layout",
            ToDaemon::SwitchConsole { .. } => "switch_console",
            ToDaemon::Pong { .. } => "pong",
        }
    }
}

/// Who `Begin` is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Who {
    /// Chosen from the list: must be one of `State::users`.
    User {
        name: String,
    },
    /// Typed under "Other user…": any name, root included.
    TypedUser {
        name: String,
    },
    Guest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Sleep,
    Restart,
    ShutDown,
}

// ---------------------------------------------------------------------------
// secrets

/// The longest secret accepted, in bytes. Storage for it is reserved up
/// front so that the bytes are never moved (a move would leave a copy).
pub const SECRET_MAX: usize = 1024;

/// Typed secret bytes (UTF-8). Overwritten when dropped; `Debug` hides them.
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    /// Copy `text` into new storage. Text longer than [`SECRET_MAX`] bytes is
    /// cut at a character boundary.
    pub fn copy_from(text: &str) -> Secret {
        let mut end = text.len().min(SECRET_MAX);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(SECRET_MAX));
        bytes.extend_from_slice(&text.as_bytes()[..end]);
        Secret(bytes)
    }

    /// Copy raw bytes into new storage, at most [`SECRET_MAX`] of them.
    pub fn from_bytes(bytes: &[u8]) -> Secret {
        let end = bytes.len().min(SECRET_MAX);
        let mut storage = Zeroizing::new(Vec::with_capacity(SECRET_MAX));
        storage.extend_from_slice(&bytes[..end]);
        Secret(storage)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}
