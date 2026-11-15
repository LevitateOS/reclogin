//! What the login screen shows that comes from the system, put together.
//!
//! Users, sessions, layouts, the last user and last sessions (`last.rs`),
//! hostname and clock style are read from the system each time a greeter
//! starts or the login service's view changes. Who is logged in and which
//! power actions the login service allows come from its helper
//! ([`crate::logind`]): while that is not reporting nobody is marked as
//! logged in and no power action is offered. The marks and the running
//! sessions are the local ones (`seat0`, with a console); who is logged in
//! at all (`others_logged_in`) counts everyone.
//!
//! Built to fit one packet with room to spare ([`fit`]): at most
//! [`USERS_MAX`] users and a few dozen of everything else, each name and
//! label cut to a sane length at a character boundary; what was cut is
//! logged. A user or session whose id is too long to cut (it is what a login
//! names) is left out instead.

use std::path::Path;

use reclogin::config::Config;
use reclogin::log;
use reclogin::protocol::{RunningSession, Session, SessionKind, State, ToGreeter};
use reclogin::wire::{Channel, MESSAGE_MAX};

use crate::greeter::cut;
use crate::WHO;

use crate::last::Last;
use crate::logind::{SeatSession, View};
use crate::{host, keyboard, last, sessions, users};

pub fn read(config: &Config, login: &View) -> State {
    let layouts = keyboard::configured(
        Path::new(keyboard::XORG_CONF_DIR),
        Path::new(keyboard::VCONSOLE_CONF),
        Path::new(keyboard::RULES_XML),
    );
    let current_layout = layouts.first().map(|l| l.name.clone()).unwrap_or_default();
    let mut users = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"));
    for user in &mut users {
        user.logged_in = login.local().any(|s| s.user == user.name);
    }
    let sessions = sessions::installed(
        Path::new(sessions::WAYLAND_DIR),
        Path::new(sessions::X11_DIR),
    );
    let Last {
        user: last_user,
        sessions: remembered,
    } = Last::load(Path::new(last::PATH));
    // Only what can still be chosen: a removed user or desktop is forgotten
    // on the screen (the file keeps it until the next login of that user).
    let last_session_for = remembered
        .into_iter()
        .filter(|(user, id)| {
            users.iter().any(|u| &u.name == user) && sessions.iter().any(|s| &s.id == id)
        })
        .collect();
    let last_user = last_user.filter(|name| users.iter().any(|u| &u.name == name));
    let installed_sessions = sessions.clone();
    let mut state = State {
        hostname: host::hostname(),
        users,
        sessions,
        last_session_for,
        last_user,
        layouts,
        current_layout,
        power: login.power,
        guest_offered: crate::guest::offered(config),
        others_logged_in: login.users(),
        running: login
            .local()
            .map(|s| RunningSession {
                user: s.user.clone(),
                name: label(s, &installed_sessions),
                console: s.console,
                since: s.since,
            })
            .collect(),
        clock_24h: host::clock_24h(&host::time_locale(Path::new(host::LOCALE_CONF))),
        idle_seconds: config.idle_seconds,
    };
    fit(&mut state);
    state
}

/// The most users listed (more can still log in through "Other user…").
pub const USERS_MAX: usize = 100;
/// The most sessions, layouts, running sessions and logged-in names.
const OTHERS_MAX: usize = 32;
/// Names and labels shown, in characters.
const LABEL_MAX: usize = 64;
/// Ids a login names (account names, session ids, XKB names), in bytes:
/// longer ones are left out, never cut.
const ID_MAX: usize = 64;
/// The encoded state must stay below this, leaving room in the packet.
const ENCODED_MAX: usize = MESSAGE_MAX / 2;

/// Make `state` fit one packet with room to spare, and say in the log what
/// was cut or left out.
fn fit(state: &mut State) {
    let mut cuts: Vec<String> = Vec::new();
    let label = |text: &mut String, what: &str, cuts: &mut Vec<String>| {
        if text.chars().nth(LABEL_MAX).is_some() {
            *text = cut(text, LABEL_MAX);
            cuts.push(format!("{what} cut to {LABEL_MAX} characters"));
        }
    };
    let before = state.users.len();
    state.users.retain(|u| u.name.len() <= ID_MAX);
    if state.users.len() < before {
        cuts.push(format!(
            "{} users with an account name over {ID_MAX} bytes left out",
            before - state.users.len()
        ));
    }
    if state.users.len() > USERS_MAX {
        cuts.push(format!(
            "{} users left out (at most {USERS_MAX} listed)",
            state.users.len() - USERS_MAX
        ));
        state.users.truncate(USERS_MAX);
    }
    for user in &mut state.users {
        label(&mut user.real_name, "a real name", &mut cuts);
    }
    label(&mut state.hostname, "the hostname", &mut cuts);
    state.sessions.retain(|s| s.id.len() <= ID_MAX);
    state.sessions.truncate(OTHERS_MAX);
    for session in &mut state.sessions {
        label(&mut session.name, "a session name", &mut cuts);
    }
    state.layouts.retain(|l| {
        l.name.len() <= ID_MAX
            && l.xkb_layout.len() <= ID_MAX
            && l.xkb_variant.len() <= ID_MAX
            && l.xkb_model.len() <= ID_MAX
            && l.xkb_options.len() <= 4 * ID_MAX
    });
    state.layouts.truncate(OTHERS_MAX);
    for layout in &mut state.layouts {
        label(&mut layout.label, "a layout name", &mut cuts);
    }
    if !state.layouts.iter().any(|l| l.name == state.current_layout) {
        state.current_layout = state
            .layouts
            .first()
            .map(|l| l.name.clone())
            .unwrap_or_default();
    }
    state.running.retain(|r| r.user.len() <= ID_MAX);
    state.running.truncate(OTHERS_MAX);
    for running in &mut state.running {
        label(&mut running.name, "a running session's name", &mut cuts);
    }
    state.others_logged_in.retain(|u| u.len() <= ID_MAX);
    state.others_logged_in.truncate(OTHERS_MAX);
    let listed = |state: &State, user: &str| state.users.iter().any(|u| u.name == user);
    let kept: Vec<_> = std::mem::take(&mut state.last_session_for)
        .into_iter()
        .filter(|(user, id)| listed(state, user) && state.sessions.iter().any(|s| &s.id == id))
        .collect();
    state.last_session_for = kept.into_iter().collect();
    if state.last_user.as_ref().is_some_and(|u| !listed(state, u)) {
        state.last_user = None;
    }
    // Whatever is still too long (it cannot be with the limits above):
    // fewer users until it fits.
    let encoded = |state: &State| {
        Channel::encode(&ToGreeter::State(Box::new(state.clone())), 0)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    };
    let mut dropped = 0;
    while encoded(state) >= ENCODED_MAX && !state.users.is_empty() {
        let keep = state.users.len() / 2;
        dropped += state.users.len() - keep;
        state.users.truncate(keep);
    }
    if dropped > 0 {
        cuts.push(format!("{dropped} more users left out to fit one message"));
        let users = state.users.clone();
        state
            .last_session_for
            .retain(|user, _| users.iter().any(|u| &u.name == user));
        if state
            .last_user
            .as_ref()
            .is_some_and(|u| !users.iter().any(|x| &x.name == u))
        {
            state.last_user = None;
        }
    }
    if !cuts.is_empty() {
        cuts.dedup();
        log(
            WHO,
            format_args!("the login screen's state was cut: {}", cuts.join("; ")),
        );
    }
}

/// What the return screen calls a running session: the name of the
/// installed desktop it was registered as ("Plasma (Wayland)"), or what the
/// login service says it is.
///
/// The login service holds `Type` (`wayland`, `x11`, `tty`) and `Desktop`
/// (what the session was registered with: the session file's first
/// `DesktopNames=` entry, else its id). The installed session whose
/// registration name equals `Desktop` and whose kind equals `Type` gives its
/// `Name=`; with none, the desktop and kind in plain words.
fn label(running: &SeatSession, installed: &[Session]) -> String {
    let kind = match running.kind.as_str() {
        "wayland" => SessionKind::Wayland,
        "x11" => SessionKind::X11,
        "tty" => return "Text console".into(),
        _ => return running.desktop.clone(),
    };
    let desktop = running.desktop.trim();
    if !desktop.is_empty() {
        let found = installed.iter().find(|s| {
            s.kind == kind
                && registration_name(s)
                    .map(|name| name.eq_ignore_ascii_case(desktop))
                    .unwrap_or(false)
        });
        if let Some(session) = found {
            return session.name.clone();
        }
    }
    let kind = match kind {
        SessionKind::Wayland => "Wayland",
        SessionKind::X11 => "X11",
    };
    if desktop.is_empty() {
        format!("Desktop ({kind})")
    } else {
        format!("{desktop} ({kind})")
    }
}

/// What a session was registered as: the first `DesktopNames=` entry of its
/// file, else its id (what `session.rs` puts in `XDG_SESSION_DESKTOP`).
fn registration_name(session: &Session) -> Option<String> {
    let dir = match session.kind {
        SessionKind::Wayland => sessions::WAYLAND_DIR,
        SessionKind::X11 => sessions::X11_DIR,
    };
    let text =
        std::fs::read_to_string(Path::new(dir).join(format!("{}.desktop", session.id))).ok()?;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(("DesktopNames", value)) = line.split_once('=').map(|(k, v)| (k.trim(), v)) {
            if let Some(first) = value.split(';').map(str::trim).find(|s| !s.is_empty()) {
                return Some(first.to_string());
            }
        }
    }
    Some(session.id.clone())
}
