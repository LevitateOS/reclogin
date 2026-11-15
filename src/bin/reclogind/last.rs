//! What reclogind remembers between logins: the last user who logged in and
//! the session each user used last, in `/var/lib/reclogin/last.json`.
//!
//! The file is root's (the folder 0700, the file 0600) and is replaced
//! atomically: written to a new file beside it, flushed, then renamed over
//! the old one. A missing or damaged file is the same as an empty one.
//! Only users in the login list are recorded, so the file cannot grow with
//! names typed under "Other user…".
//!
//! Written when a session has really started (the worker said `started`,
//! `login.rs`), with the session `login.rs` chose for it: a wrong password
//! or a session that did not start changes nothing.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use reclogin::log;
use serde::{Deserialize, Serialize};

use crate::{users, WHO};

pub const PATH: &str = "/var/lib/reclogin/last.json";

/// The most users the file keeps (a machine with more has far more list
/// than a login screen can show).
const MAX_USERS: usize = 1024;
const MAX_FILE: u64 = 1 << 20;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Last {
    /// The user who logged in last.
    #[serde(default)]
    pub user: Option<String>,
    /// User name to `Session::id`.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
}

impl Last {
    /// The file's content; empty when it is missing, too large or damaged.
    pub fn load(path: &Path) -> Last {
        let Ok(file) = std::fs::File::open(path) else {
            return Last::default();
        };
        let mut text = String::new();
        if file.take(MAX_FILE).read_to_string(&mut text).is_err() {
            return Last::default();
        }
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("/"));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let temp: PathBuf = dir.join(format!(".last.json.{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
            return written;
        }
        // Make the rename itself durable.
        std::fs::File::open(dir)?.sync_all()
    }
}

/// A session `session` (`Session::id`) started for `user`: remembered when
/// `user` is in the login list (any other name is not). Failing to write is
/// logged and changes nothing else.
pub fn session_started(user: &str, session: &str) {
    let listed = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"))
        .iter()
        .any(|u| u.name == user);
    if !listed {
        return;
    }
    let path = Path::new(PATH);
    let mut last = Last::load(path);
    last.user = Some(user.to_string());
    if last.sessions.contains_key(user) || last.sessions.len() < MAX_USERS {
        last.sessions.insert(user.to_string(), session.to_string());
    }
    if let Err(e) = last.save(path) {
        log(WHO, format_args!("remembering the login in {PATH}: {e}"));
    }
}
