//! What the login screen shows that comes from the system, put together.
//!
//! Users, sessions, layouts, the last user and last sessions (`last.rs`),
//! hostname and clock style are read from the system each time a greeter
//! starts. Who is logged in and which power actions logind allows are not
//! asked yet (the system bus is used from the stage that adds them, at
//! `/run/dbus/system_bus_socket`): nobody is logged in and no power action
//! is offered.

use std::path::Path;

use reclogin::config::Config;
use reclogin::protocol::{Power, State};

use crate::last::Last;
use crate::{host, keyboard, last, sessions, users};

pub fn read(config: &Config) -> State {
    let layouts = keyboard::configured(
        Path::new(keyboard::XORG_CONF_DIR),
        Path::new(keyboard::VCONSOLE_CONF),
        Path::new(keyboard::RULES_XML),
    );
    let current_layout = layouts.first().map(|l| l.name.clone()).unwrap_or_default();
    let users = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"));
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
    State {
        hostname: host::hostname(),
        users,
        sessions,
        last_session_for,
        last_user,
        layouts,
        current_layout,
        power: Power::default(),
        guest_offered: config.guest,
        others_logged_in: Vec::new(),
        clock_24h: host::clock_24h(&host::time_locale(Path::new(host::LOCALE_CONF))),
        idle_seconds: config.idle_seconds,
    }
}
