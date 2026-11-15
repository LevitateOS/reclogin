//! What the login screen shows that comes from the system, put together.
//!
//! Users, sessions, layouts, the last user and last sessions (`last.rs`),
//! hostname and clock style are read from the system each time a greeter
//! starts or the login service's view changes. Who is logged in and which
//! power actions the login service allows come from its helper
//! ([`crate::logind`]): while that is not reporting nobody is marked as
//! logged in and no power action is offered.

use std::path::Path;

use reclogin::config::Config;
use reclogin::protocol::{RunningSession, Session, SessionKind, State};

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
        user.logged_in = login.sessions.iter().any(|s| s.user == user.name);
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
    State {
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
            .sessions
            .iter()
            .map(|s| RunningSession {
                user: s.user.clone(),
                name: label(s, &installed_sessions),
                console: s.console,
                since: s.since,
            })
            .collect(),
        clock_24h: host::clock_24h(&host::time_locale(Path::new(host::LOCALE_CONF))),
        idle_seconds: config.idle_seconds,
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
