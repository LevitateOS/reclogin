//! What the login screen shows that comes from the system, put together.
//!
//! Stage 1: users, sessions, layouts, hostname and clock style are read from
//! the system. Who is logged in and which power actions logind allows are
//! not asked yet (the system bus is used from the stage that adds them,
//! at `/run/dbus/system_bus_socket`): nobody is logged in and no power
//! action is offered. The last user and last sessions are not remembered
//! yet.

use std::path::Path;

use reclogin::config::Config;
use reclogin::protocol::{Power, State};

use crate::{host, keyboard, sessions, users};

pub fn read(config: &Config) -> State {
    let layouts = keyboard::configured(
        Path::new(keyboard::XORG_CONF_DIR),
        Path::new(keyboard::RULES_XML),
    );
    let current_layout = layouts.first().map(|l| l.name.clone()).unwrap_or_default();
    State {
        hostname: host::hostname(),
        users: users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells")),
        sessions: sessions::installed(
            Path::new(sessions::WAYLAND_DIR),
            Path::new(sessions::X11_DIR),
        ),
        last_session_for: Default::default(),
        last_user: None,
        layouts,
        current_layout,
        power: Power::default(),
        guest_offered: config.guest,
        others_logged_in: Vec::new(),
        clock_24h: host::clock_24h(&host::time_locale(Path::new(host::LOCALE_CONF))),
        idle_seconds: config.idle_seconds,
    }
}
