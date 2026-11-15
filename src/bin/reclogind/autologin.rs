//! Automatic login (`docs/reclogin.md`, decision 2; stage 6 of the plan).
//!
//! `/etc/reclogin.conf` names an account (or `guest`) in `autologin` and,
//! optionally, a session in `autologin_session`. At `reclogind`'s start, and
//! only then, that session is started without a password and without a
//! countdown. There is no second chance: if it fails, or when the session
//! ends, the login screen is what is shown.
//!
//! How nobody else gets in through this (the whole of the safety argument):
//!
//! 1. Only `reclogind`'s start-up sends the worker the message
//!    `start_auto` ([`Daemon::start_autologin`]). The greeter has no message
//!    that leads here, and the worker's socket is private to `reclogind`.
//! 2. The account is the one the root-owned `/etc/reclogin.conf` names:
//!    never typed, never from the greeter. A name that is not on the user
//!    list (root, system accounts, accounts without a login shell or home)
//!    is refused by `reclogind` and again by the worker, which looks the
//!    account up itself. `guest` is the exception and goes through the
//!    guest's own path (`guest.rs`), checks and cleanup included.
//! 3. Once per boot: a mark file `/run/reclogin/autologin-done` is created
//!    (exclusively) before anything is tried. A restarted `reclogind` finds
//!    it and does not log in again, so a crash of the login screen cannot be
//!    used to start the session again, nor can a logout.
//! 4. PAM's service `reclogin-autologin` authenticates nobody by itself
//!    (`pam_permit` for auth), so it must only ever be reached through the
//!    worker's [`worker_login`], which has no conversation: a question from
//!    any module is refused. Its account and session lines are those of
//!    `reclogin`: a locked or expired account, or one outside its allowed
//!    time, is refused there as in a normal login. An account whose password
//!    has expired is refused too (changing it needs the login screen).
//!
//! The sentence the screen shows on a failure is `Entry::line`: "Automatic
//! login did not work." and the reason.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::Result;
use reclogin::config::Config;
use reclogin::log;
use reclogin::protocol::Session;
use reclogin::wire::Channel;

use crate::last::{self, Last};
use crate::login::Entry;
use crate::pam::{self, Pam};
use crate::session::{self, Account};
use crate::worker::{FromWorker, SessionRef, Talk};
use crate::{guest, sessions, users, Daemon};

const WHO: &str = "reclogind (automatic login)";
const WORKER_WHO: &str = "reclogind (automatic login worker)";

/// PAM's service for the automatic login; see `dist/pam/`.
const SERVICE: &str = "reclogin-autologin";

/// Created once per boot, before the first attempt.
const DONE_MARK: &str = "/run/reclogin/autologin-done";

const PREFIX: &str = "Automatic login did not work.";
const NOT_LISTED: &str = "That account is not on the list of users, so it cannot log in this way.";
const ROOT: &str = "Root cannot log in this way.";
const REFUSED: &str = "The account was refused.";
const EXPIRED: &str = "The account's password has expired. Log in below to change it.";
const NOT_STARTED: &str = "The session could not be started.";
const NO_MARK: &str = "It could not be recorded that it was tried.";
const NOTHING_INSTALLED: &str = "No desktop session is installed.";

impl Entry {
    /// The line for the screen: the failure `reason` of a login, with the
    /// automatic login's own sentence before it when it was one.
    pub fn line(self, reason: &str) -> String {
        match self {
            Entry::Typed => reason.to_string(),
            Entry::Automatic if reason.is_empty() => PREFIX.to_string(),
            Entry::Automatic => format!("{PREFIX} {reason}"),
        }
    }
}

/// Remember, for this boot, that automatic login was tried. `Ok(false)`:
/// it was already (this is a restarted `reclogind`).
fn claim_once() -> io::Result<bool> {
    guest::ensure_run_dir()?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(DONE_MARK)
    {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// The session to start: `autologin_session` when installed; else the
/// user's last session when installed; else the first installed one. `None`
/// when nothing is installed.
fn choose_session(wanted: Option<&str>, user: &str, installed: &[Session]) -> Option<String> {
    let has = |id: &str| installed.iter().any(|s| s.id == id);
    if let Some(id) = wanted.map(str::trim).filter(|id| !id.is_empty()) {
        if has(id) {
            return Some(id.to_string());
        }
        log(
            WHO,
            format_args!("autologin_session {id:?} is not installed; choosing another"),
        );
    }
    let last = Last::load(Path::new(last::PATH));
    if let Some(id) = last.sessions.get(user).filter(|id| has(id)) {
        return Some(id.clone());
    }
    installed.first().map(|s| s.id.clone())
}

impl Daemon {
    /// At `reclogind`'s start, with the greeter up: start the automatic
    /// session, or show why not. Called once, from `run`.
    pub(crate) fn start_autologin(&mut self, config: &Config, screen_free: bool) {
        let Some(name) = config.autologin.as_deref().map(str::trim) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        match claim_once() {
            Ok(true) => {}
            Ok(false) => {
                log(
                    WHO,
                    "automatic login was already tried since the system started; not again",
                );
                return;
            }
            Err(e) => {
                log(WHO, format_args!("{DONE_MARK}: {e}"));
                if screen_free {
                    let _ = self.refuse(&Entry::Automatic.line(NO_MARK));
                }
                return;
            }
        }
        if !screen_free {
            // A user session's console is in front: someone is logged in
            // already, which is what automatic login is for.
            log(WHO, "a session is in front already; no automatic login");
            return;
        }

        let installed = sessions::installed(
            Path::new(sessions::WAYLAND_DIR),
            Path::new(sessions::X11_DIR),
        );
        let (user, is_guest) = if name == guest::NAME {
            (guest::NAME.to_string(), true)
        } else {
            let listed = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"))
                .iter()
                .any(|u| u.name == name);
            if !listed {
                log(
                    WHO,
                    format_args!("autologin = {name}: not on the user list; refused"),
                );
                let _ = self.refuse(&Entry::Automatic.line(NOT_LISTED));
                return;
            }
            (name.to_string(), false)
        };
        if is_guest {
            if let Some(text) = self.login.guest.refusal() {
                log(
                    WHO,
                    format_args!("autologin = guest refused ({:?})", self.login.guest),
                );
                let _ = self.refuse(&Entry::Automatic.line(text));
                return;
            }
        }
        let Some(session) = choose_session(config.autologin_session.as_deref(), &user, &installed)
        else {
            let _ = self.refuse(&Entry::Automatic.line(NOTHING_INSTALLED));
            return;
        };
        log(
            WHO,
            format_args!("starting {session} for {user}, without a password"),
        );
        let _ = self.start_login(user, is_guest, Entry::Automatic, Some(session));
    }
}

/// The whole automatic login, in the session worker, for a normal account
/// (the guest's goes through `guest::worker_login`). No question is ever
/// answered: this service has no conversation.
pub fn worker_login(
    channel: &Channel,
    signals: &OwnedFd,
    user: &str,
    session: &SessionRef,
    console: u32,
) -> Result<()> {
    let fail = |text: &str| channel.send(&FromWorker::Failed { text: text.into() }, &[]);

    // The same rule as the user list, checked again here: this process does
    // not take reclogind's word for it. Root is never listed.
    let listed = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"))
        .iter()
        .any(|u| u.name == user);
    if user == guest::NAME || !listed {
        log(
            WORKER_WHO,
            format_args!("{user:?} is not on the user list; refused"),
        );
        fail(NOT_LISTED)?;
        return Ok(());
    }
    let account = match Account::lookup(user) {
        Ok(account) => account,
        Err(e) => {
            log(WORKER_WHO, format_args!("account {user}: {e}"));
            fail(REFUSED)?;
            return Ok(());
        }
    };
    if account.uid == 0 || account.uid == guest::UID || account.name != user {
        log(
            WORKER_WHO,
            format_args!("{user:?} is uid {}; refused", account.uid),
        );
        fail(if account.uid == 0 { ROOT } else { NOT_LISTED })?;
        return Ok(());
    }

    let plan = match session::read(session) {
        Ok(plan) => plan,
        Err(text) => {
            fail(&text)?;
            return Ok(());
        }
    };

    let mut pam = match Pam::start(SERVICE, user, Talk::silent(channel)) {
        Ok(pam) => pam,
        Err(e) => {
            log(WORKER_WHO, e);
            fail(NOT_STARTED)?;
            return Ok(());
        }
    };
    let rc = pam.set_item(pam::TTY, &format!("tty{console}"));
    if rc != pam::SUCCESS {
        log(
            WORKER_WHO,
            format_args!("setting PAM_TTY: {}", pam.describe(rc)),
        );
        fail(NOT_STARTED)?;
        return Ok(());
    }
    let rc = pam.authenticate();
    if pam.talker().cancelled {
        return Ok(());
    }
    if rc != pam::SUCCESS {
        log(
            WORKER_WHO,
            format_args!("{SERVICE} refused: {}", pam.describe(rc)),
        );
        fail(REFUSED)?;
        return Ok(());
    }
    let rc = pam.acct_mgmt();
    if pam.talker().cancelled {
        return Ok(());
    }
    if rc != pam::SUCCESS {
        log(
            WORKER_WHO,
            format_args!("{SERVICE} refused the account: {}", pam.describe(rc)),
        );
        fail(if rc == pam::NEW_AUTHTOK_REQD {
            EXPIRED
        } else {
            REFUSED
        })?;
        return Ok(());
    }
    // A module may change the name: this service is for the one account.
    if pam.user().as_deref() != Some(user) {
        log(
            WORKER_WHO,
            format_args!("{SERVICE} changed the user to {:?}; refused", pam.user()),
        );
        fail(REFUSED)?;
        return Ok(());
    }

    channel.send(&FromWorker::Accepted, &[])?;
    session::run(&mut pam, &account, &plan, console, channel, signals);
    drop(pam);
    Ok(())
}
