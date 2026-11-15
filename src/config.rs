//! `/etc/reclogin.conf`: reclogin's only settings.
//!
//! Lines of `key = value`; `#` starts a comment. Keys:
//!
//! - `autologin`: a user name, or `guest`, logged in at boot without the
//!   login screen;
//! - `autologin_session`: the session (`.desktop` file name without the
//!   ending) for automatic login;
//! - `guest`: `on` or `off` (default `on`), whether the guest entry is offered;
//! - `idle_seconds`: switch the display off after this many seconds without
//!   input; `0` never.
//!
//! Unknown keys and unreadable values are reported and ignored: a mistake in
//! the file must never keep the login screen from starting.

use std::path::{Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/reclogin.conf";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub autologin: Option<String>,
    pub autologin_session: Option<String>,
    pub guest: bool,
    /// `None`: not set, the system's own idle setting applies.
    pub idle_seconds: Option<u64>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            autologin: None,
            autologin_session: None,
            guest: true,
            idle_seconds: None,
        }
    }
}

/// The settings and what was wrong with the file, one line per problem.
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    pub problems: Vec<String>,
}

impl Config {
    /// Read the file at `path`. A missing file gives the defaults without a
    /// problem; an unreadable one gives the defaults with one.
    pub fn load(path: &Path) -> Loaded {
        let mut problems = Vec::new();
        let config = match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, &mut problems),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                problems.push(format!(
                    "cannot read {}: {e}; using the defaults",
                    path.display()
                ));
                Config::default()
            }
        };
        let problems = problems
            .into_iter()
            .map(|p| format!("{}: {p}", path.display()))
            .collect();
        Loaded {
            config,
            path: path.to_path_buf(),
            problems,
        }
    }

    pub fn parse(text: &str, problems: &mut Vec<String>) -> Config {
        let mut config = Config::default();
        for (number, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = number + 1;
            let Some((key, value)) = line.split_once('=') else {
                problems.push(format!("line {at}: not `key = value`, ignored"));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "autologin" => config.autologin = (!value.is_empty()).then(|| value.to_string()),
                "autologin_session" => {
                    config.autologin_session = (!value.is_empty()).then(|| value.to_string())
                }
                "guest" => match value {
                    "on" | "yes" | "true" => config.guest = true,
                    "off" | "no" | "false" => config.guest = false,
                    _ => problems.push(format!(
                        "line {at}: guest must be on or off, not {value:?}; ignored"
                    )),
                },
                "idle_seconds" => match value.parse::<u64>() {
                    Ok(n) => config.idle_seconds = Some(n),
                    Err(_) => problems.push(format!(
                        "line {at}: idle_seconds must be a whole number, not {value:?}; ignored"
                    )),
                },
                _ => problems.push(format!("line {at}: unknown key {key:?}, ignored")),
            }
        }
        config
    }
}
