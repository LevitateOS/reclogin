//! The guest account (`docs/reclogin-guest.md`, stage 7 of the plan).
//!
//! One fixed account, `guest`, uid and gid 60600, made by the installed
//! tree's sysusers line, never at login. Its home is a fresh memory file
//! system at `/run/reclogin/guest` for each login, unmounted at logout. One
//! cleanup routine ([`cleanup_locked`]) runs at `reclogind`'s start, before
//! every guest login and after every guest logout; a guest login only starts
//! when it finished completely.
//!
//! Who does what:
//!
//! - `reclogind` at start ([`at_start`]): reads the setting and checks that
//!   the account is really the guest. Then ([`Daemon::start_guest_cleanup`])
//!   the cleanup runs in a child process, `reclogind --guest-cleanup`, also
//!   with the entry switched off (so that leftovers go), with the search for
//!   set-id files of the guest on every file system that allows them (only
//!   at start; see [`strip_setid_everywhere`]). The screen does not offer
//!   Guest until it finished clean ([`offered`]).
//! - `begin {who: guest}` (`login.rs`): refused with a plain line when the
//!   setting is off, the account is not the guest, or a cleanup runs or
//!   failed; otherwise a session worker is started with `start_guest`.
//! - The session worker ([`worker_login`]): takes the guest lock, runs the
//!   cleanup (without the login service), mounts the home and fills it as
//!   the guest, runs PAM's `reclogin-guest` service for the account `guest`
//!   only and without a conversation, then the usual path (`session::run`:
//!   console, X or Wayland, the desktop, its end). The desktop and the X
//!   server of a guest make no set-id file (`nosetid.rs`). After the session
//!   it runs the cleanup again (ending every process of the guest and
//!   unmounting the home) and only then exits.
//! - After every guest worker, however it ended, `reclogind` runs the
//!   cleanup child again. It is the only place that asks the login service
//!   to end the guest (`TerminateUser`): the worker sits in the guest's own
//!   session scope on LevitateOS and would be ended by its own request.
//!
//! `reclogind`'s main process stays single-threaded: every cleanup it wants
//! runs in the child process, which it reaps from its main loop without
//! waiting ([`Daemon::reap_guest_cleanup`]).
//!
//! One guest at a time: the guest lock, `/run/reclogin/guest.lock`, a `flock`
//! held by the guest's worker from before its first cleanup to the end of its
//! logout cleanup, and by a cleanup child while it runs (`reclogind` takes it
//! and hands it to the child as descriptor 3). The kernel drops it when its
//! holder dies, so a crashed worker never blocks the next guest. Its content
//! names the holder (`session <pid>` or `cleanup <pid>`, always 32 bytes,
//! written in one call over the old text, never emptied); a reader trusts it
//! only when that process is alive and is the worker or the cleanup child,
//! and otherwise takes the lock as "being cleared". The lock is how step 1
//! ("if the login service lists a session of user 60600, stop") is answered
//! here: a guest session reclogin started has a live worker holding it.
//!
//! "Finished completely" means the check of step 7 found nothing: no process
//! whose real or saved uid is 60600 (zombies aside), nothing of the guest's
//! mounted, none of the named leftovers or scheduled jobs, nothing of uid
//! 60600 in the shared places, no per-user journal or core dump of uid 60600,
//! no System V object of uid 60600. A failed check leaves the mark
//! `/run/reclogin/guest-cleanup-failed` (removed by the next clean pass);
//! while it is there [`offered`] is false and `reclogind` runs the cleanup
//! child again after a pause that doubles each time (one minute, at most
//! thirty).

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use reclogin::config::Config;
use reclogin::log;
use reclogin::protocol::ToGreeter;
use reclogin::wire::Channel;

use crate::pam::{self, Pam};
use crate::session::{self, Account};
use crate::worker::{hold, FromWorker, SessionRef, Talk};
use crate::{Daemon, Next};

const WHO: &str = "reclogind (guest)";

/// The account (design point 1; owner's decision 6 for the number).
pub const NAME: &str = "guest";
pub const UID: libc::uid_t = 60600;
pub const GID: libc::gid_t = 60600;
pub const HOME: &str = "/run/reclogin/guest";
const HOME_C: &CStr = c"/run/reclogin/guest";

/// The account whose uid the sweep helper keeps as its real and saved uid
/// (`dist/sysusers/reclogin.conf`): nobody else has it, so the guest's
/// processes cannot signal the helper, while it, with 60600 as its
/// effective uid, can signal them (see [`kill_as_guest`]).
const SWEEP_NAME: &str = "reclogin-sweep";
const SWEEP_UID: libc::uid_t = 60601;

/// The PAM service of the guest (design point 4).
const SERVICE: &str = "reclogin-guest";

const RUN_DIR: &str = "/run/reclogin";
const LOCK_PATH: &str = "/run/reclogin/guest.lock";
const FAILED_MARK: &str = "/run/reclogin/guest-cleanup-failed";
/// Written when the search for set-id files of the guest finished clean
/// this boot (`/run` is empty at every boot).
const SETID_CHECKED_MARK: &str = "/run/reclogin/guest-setid-checked";

/// The hidden switch of the cleanup child, `reclogind --guest-cleanup`,
/// which gets the guest lock as descriptor [`LOCK_FD`]; with
/// [`SETID_WALK_ARG`] it also searches for set-id files.
pub const CLEANUP_SWITCH: &str = "--guest-cleanup";
const SETID_WALK_ARG: &str = "--setid-walk";
const LOCK_FD: RawFd = 3;
/// The holder line's fixed length.
const HOLDER_LEN: usize = 32;

/// Copied over the fresh home, in this order (design point 2, step 4).
const DEFAULTS: [&str; 2] = ["/etc/skel", "/usr/share/reclogin/guest-home"];

/// The home's size: a quarter of memory, at most 4 GiB (owner's decision 5).
const HOME_CAP: u64 = 4 << 30;
/// How long the uid sweep repeats before it gives up (design point 3).
const SWEEP_FOR: Duration = Duration::from_secs(10);
/// How long one sweep helper may take before it is killed.
const SWEEP_HELPER_WAIT: Duration = Duration::from_secs(3);
/// How many sweep helpers may fail before the sweep gives up.
const SWEEP_TRIES: u32 = 3;
/// How long the removal walks of one cleanup, and its check, may take.
const WALK_BUDGET: Duration = Duration::from_secs(30);
/// How long the search for set-id files may take (at start only).
const SETID_BUDGET: Duration = Duration::from_secs(60);
/// How often one walk may begin again because a folder moved under it.
const MAX_RESTARTS: u32 = 64;
/// How long the copy of the defaults into the home may take.
const COPY_WAIT: Duration = Duration::from_secs(60);
/// How long a worker started with `wait` waits for a running cleanup.
const WAIT_FOR_CLEANUP: Duration = Duration::from_secs(110);
/// The pauses before a failed cleanup runs again.
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MOST: Duration = Duration::from_secs(30 * 60);
const TMPFS_MAGIC: u64 = 0x0102_1994;

// What the screen says.
pub const OFF: &str = "The guest session is switched off.";
pub const UNAVAILABLE: &str = "The guest session is not available.";
pub const STILL_CLEARING: &str = "The guest session is still being cleared. Try again in a moment.";
pub const ALREADY_RUNNING: &str = "A guest session is already running.";
const NOT_STARTED: &str = "The guest session could not be started.";

// ---------------------------------------------------------------------------
// The one list of places a guest can leave something (design point 3, steps
// 3 to 5). A new place is one new line here.

/// Unmounted (deepest first): the home, the drives the guest mounted, the
/// login service's runtime folder. Besides these, a FUSE mount the guest
/// made (`user_id=60600`) under one of [`SHARED_PLACES`].
const MOUNTED_UNDER: [&str; 3] = [HOME, "/run/media/guest", "/run/user/60600"];

/// Everything of uid 60600 in these is removed; the walk never follows a
/// symbolic link and stays on the place's own file system.
const SHARED_PLACES: [&str; 4] = ["/tmp", "/var/tmp", "/dev/shm", "/dev/mqueue"];

/// Scheduled jobs, removed before the sweep so that nothing starts a new
/// process of the guest's after it: crontabs (cronie on LevitateOS, busybox
/// crond on AcornOS) by name, and `at` jobs owned by uid 60600 in the spools.
const CRONTABS: [&str; 2] = ["/var/spool/cron/guest", "/var/spool/cron/crontabs/guest"];
const AT_SPOOLS: [&str; 3] = [
    "/var/spool/at",
    "/var/spool/cron/atjobs",
    "/var/spool/cron/atspool",
];

/// Removed whole when present (after the unmounting).
const LEFTOVERS: [&str; 7] = [
    HOME,
    "/run/media/guest",
    "/run/user/60600",
    "/var/lib/systemd/linger/guest",
    "/var/lib/elogind/linger/guest",
    "/var/lib/AccountsService/users/guest",
    "/var/lib/AccountsService/icons/guest",
];

/// The system's own records that uid 60600 can read (LevitateOS): the
/// per-user journal files (`user-60600.journal`, `user-60600@….journal`) in
/// each machine's folder under these, and systemd-coredump's dumps of uid
/// 60600 (`core.<command>.60600.<boot id>.<pid>.<time>…`).
const JOURNAL_DIRS: [&str; 2] = ["/var/log/journal", "/run/log/journal"];
const COREDUMP_DIR: &str = "/var/lib/systemd/coredump";

// ---------------------------------------------------------------------------
// the login service

/// Ask the login service to end the user's sessions and services.
///
/// On the system bus: destination `org.freedesktop.login1`, object
/// `/org/freedesktop/login1`, interface `org.freedesktop.login1.Manager`,
/// method `TerminateUser(u uid)`. Both systemd-logind and elogind provide
/// it; on LevitateOS it also stops the guest's `systemd --user`.
///
/// Only the cleanup child calls it (never the worker, which the login
/// service puts into the guest's own session scope on LevitateOS). The bus
/// library starts threads, so the call is made by a one-shot process,
/// `reclogind --end-user UID` (`loginhelper::end_user`), waited for at most
/// [`END_USER_WAIT`]. Every failure is ignored: this is a courtesy, the uid
/// sweep after it is the guarantee.
fn ask_login_service_to_end_user(uid: libc::uid_t) {
    let mut command = Command::new("/proc/self/exe");
    command
        .arg(crate::loginhelper::END_USER)
        .arg(uid.to_string())
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .current_dir("/");
    unsafe {
        command.pre_exec(|| {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            log(
                WHO,
                format_args!("asking the login service to end user {uid}: {e}"),
            );
            return;
        }
    };
    let deadline = Instant::now() + END_USER_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                log(
                    WHO,
                    format_args!(
                        "TerminateUser({uid}) took over {} seconds; going on",
                        END_USER_WAIT.as_secs()
                    ),
                );
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(_) => return,
        }
    }
}

/// How long the cleanup waits for `reclogind --end-user`.
const END_USER_WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// the setting and the account

/// Whether `begin {who: guest}` may start a guest, as far as the setting
/// and the account go (whether a cleanup runs or failed is asked each time:
/// [`not_ready`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Setting {
    /// Not looked at (refused, as a safe default).
    #[default]
    Unknown,
    /// `guest = off` in `/etc/reclogin.conf`.
    Off,
    /// The account is missing or is not reclogin's guest.
    Unavailable,
    On,
}

impl Setting {
    /// The line for the screen when a guest is refused.
    pub fn refusal(self) -> Option<&'static str> {
        match self {
            Setting::On => None,
            Setting::Off => Some(OFF),
            Setting::Unknown | Setting::Unavailable => Some(UNAVAILABLE),
        }
    }

    /// The settings the screen is drawn from: the Guest entry only when a
    /// guest may start (and, each time the state is built, [`offered`]).
    pub fn config_for_screen(self, config: &Config) -> Config {
        Config {
            guest: self == Setting::On,
            ..config.clone()
        }
    }
}

/// The account `guest`, checked to be reclogin's guest: uid and gid 60600,
/// home `/run/reclogin/guest`, no other account with that number, and a
/// locked shadow password. Anything else (an old account called `guest`)
/// is never used: no tmpfs over a person's home, no sweep of their processes.
pub fn account() -> Result<Account, String> {
    let account = Account::lookup(NAME).map_err(|e| format!("no account {NAME}: {e}"))?;
    if account.uid != UID || account.gid != GID {
        return Err(format!(
            "the account {NAME} has uid {} and gid {}, not {UID} and {GID}: it is not reclogin's guest",
            account.uid, account.gid
        ));
    }
    if account.home != HOME {
        return Err(format!(
            "the account {NAME} has the home {}, not {HOME}: it is not reclogin's guest",
            account.home
        ));
    }
    let others = others_with_uid(UID, NAME)?;
    if !others.is_empty() {
        return Err(format!(
            "uid {UID} also belongs to {}: the guest's processes could not be told apart",
            others.join(", ")
        ));
    }
    let shadow = fs::read_to_string("/etc/shadow").map_err(|e| format!("/etc/shadow: {e}"))?;
    let password = shadow
        .lines()
        .find_map(|line| line.strip_prefix("guest:"))
        .and_then(|rest| rest.split(':').next());
    match password {
        Some(p) if p.starts_with('!') || p.starts_with('*') => Ok(account),
        Some(_) => Err(format!(
            "the account {NAME} has a password; reclogin's guest is locked (!*)"
        )),
        None => Err(format!("the account {NAME} has no line in /etc/shadow")),
    }
}

/// The names in `/etc/passwd`, other than `name`, that have uid `uid`.
fn others_with_uid(uid: libc::uid_t, name: &str) -> Result<Vec<String>, String> {
    let passwd = fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
    let number = uid.to_string();
    Ok(passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() >= 3 && fields[2] == number && fields[0] != name)
                .then(|| fields[0].to_string())
        })
        .collect())
}

/// [`account`]'s answer, read again only when `/etc/passwd`, `/etc/shadow`
/// or `/etc/group` changed (their inode and change time): [`offered`] is
/// asked each time the state is built.
fn account_ok() -> bool {
    /// Inode and change time of each file the answer was read from.
    type Seen = Vec<(u64, i64, i64)>;
    static CACHE: Mutex<Option<(Seen, bool)>> = Mutex::new(None);
    let key: Seen = ["/etc/passwd", "/etc/shadow", "/etc/group"]
        .iter()
        .map(|p| {
            fs::metadata(p)
                .map(|m| (m.ino(), m.ctime(), m.ctime_nsec()))
                .unwrap_or((0, 0, 0))
        })
        .collect();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((seen, ok)) = cache.as_ref() {
        if *seen == key {
            return *ok;
        }
    }
    let ok = account().is_ok();
    *cache = Some((key, ok));
    ok
}

/// At `reclogind`'s start, before the screen is shown: the setting and the
/// account. The cleanup itself is started by `run` right after
/// ([`Daemon::start_guest_cleanup`]), also with the entry switched off, but
/// never for an account that is not the guest.
pub fn at_start(config: &Config) -> Setting {
    let account_ok = match account() {
        Ok(_) => true,
        Err(why) => {
            log(WHO, format_args!("no Guest entry: {why}"));
            false
        }
    };
    if !config.guest {
        log(WHO, "the Guest entry is switched off (guest = off)");
        Setting::Off
    } else if account_ok {
        Setting::On
    } else {
        Setting::Unavailable
    }
}

/// A cleanup child of this `reclogind` runs (main process only).
static CLEANING: AtomicBool = AtomicBool::new(false);

/// Whether the screen offers Guest now, from the current facts, for
/// `state.rs` each time the state is built: the setting is on and the
/// account is the guest (`config.guest`, from [`Setting::config_for_screen`],
/// and [`account_ok`]), no cleanup child runs, the last cleanup did not fail,
/// and the start's search for set-id files finished clean. A few `stat`s.
pub fn offered(config: &Config) -> bool {
    config.guest && account_ok() && not_ready().is_none()
}

/// Why a guest cannot start now although the setting allows it: a cleanup
/// runs, the last one failed, or the start's search is not done.
pub fn not_ready() -> Option<&'static str> {
    (CLEANING.load(Ordering::Relaxed) || cleanup_failed() || !setid_checked())
        .then_some(STILL_CLEARING)
}

/// Whether the last cleanup found something it could not remove.
pub fn cleanup_failed() -> bool {
    fs::symlink_metadata(FAILED_MARK).is_ok()
}

fn setid_checked() -> bool {
    fs::symlink_metadata(SETID_CHECKED_MARK).is_ok()
}

/// "End the guest session" (design point 5, "Screen locking"): the running
/// guest session's worker is asked to end it (`SIGTERM`, which ends the
/// desktop, closes PAM and runs the logout cleanup). For the return screen's
/// quiet button ([`Daemon::on_end_guest`]); false when no guest session
/// runs.
pub fn end_running_session() -> bool {
    let Ok(file) = File::open(LOCK_PATH) else {
        return false;
    };
    // Not held: no guest session.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    // A verified holder only: the process is alive and is a worker.
    let Some((Role::Session, pid)) = read_holder(&file) else {
        return false;
    };
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return false;
    }
    log(WHO, format_args!("ending the guest session (worker {pid})"));
    true
}

// ---------------------------------------------------------------------------
// the guest lock

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Session,
    Cleanup,
}

impl Role {
    fn word(self) -> &'static str {
        match self {
            Role::Session => "session",
            Role::Cleanup => "cleanup",
        }
    }

    /// The switch in the command line of a process that holds the lock in
    /// this role.
    fn switch(self) -> &'static str {
        match self {
            Role::Session => crate::worker::SWITCH,
            Role::Cleanup => CLEANUP_SWITCH,
        }
    }
}

/// Held while it lives; the kernel drops it when the last descriptor of the
/// open file is closed.
struct Lock(File);

enum Claim {
    Got(Lock),
    /// Someone holds it; who, when its content names a live holder.
    Held(Option<(Role, i32)>),
}

fn claim(role: Role) -> io::Result<Claim> {
    ensure_run_dir()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(LOCK_PATH)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != 0 {
        return Err(io::Error::other(format!(
            "{LOCK_PATH} is not a file of root's"
        )));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error);
        }
        return Ok(Claim::Held(read_holder(&file)));
    }
    write_holder(&file, role, std::process::id() as i32)?;
    Ok(Claim::Got(Lock(file)))
}

/// The holder line, always [`HOLDER_LEN`] bytes, in one `pwrite` over the
/// old one: the file is never truncated, so it is never empty.
fn write_holder(file: &File, role: Role, pid: i32) -> io::Result<()> {
    let mut line = format!("{} {pid}", role.word()).into_bytes();
    line.resize(HOLDER_LEN - 1, b' ');
    line.push(b'\n');
    file.write_all_at(&line, 0)
}

/// The holder the lock's content names, when that process is alive and is
/// what the role says (a worker, a cleanup child). `None` otherwise: an old
/// or half-written line, which a reader takes as "being cleared".
fn read_holder(file: &File) -> Option<(Role, i32)> {
    let mut buf = [0u8; HOLDER_LEN];
    let n = file.read_at(&mut buf, 0).ok()?;
    let text = std::str::from_utf8(&buf[..n]).ok()?;
    let mut words = text.split_whitespace();
    let role = match words.next()? {
        "session" => Role::Session,
        "cleanup" => Role::Cleanup,
        _ => return None,
    };
    let pid: i32 = words.next()?.parse().ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    cmdline
        .split(|&b| b == 0)
        .any(|arg| arg == role.switch().as_bytes())
        .then_some((role, pid))
}

/// `/run/reclogin`, root's, mode 0755 (design point 2, step 1).
pub(crate) fn ensure_run_dir() -> io::Result<()> {
    match fs::create_dir(RUN_DIR) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(RUN_DIR)?;
    if !meta.is_dir() || meta.uid() != 0 {
        return Err(io::Error::other(format!(
            "{RUN_DIR} is not a folder of root's"
        )));
    }
    if meta.mode() & 0o7777 != 0o755 {
        fs::set_permissions(RUN_DIR, Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the cleanup routine

pub enum Outcome {
    /// Finished completely: the check found nothing.
    Clean,
    /// Run, and the check found something left.
    Failed(String),
}

fn report(when: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Clean => log(WHO, format_args!("cleanup {when}: clean")),
        Outcome::Failed(why) => log(
            WHO,
            format_args!("CLEANUP {when} DID NOT FINISH; no guest can start until it does: {why}"),
        ),
    }
}

/// Which of the routine's optional steps run.
#[derive(Clone, Copy)]
struct Steps {
    /// `TerminateUser`: the cleanup child only (see
    /// [`ask_login_service_to_end_user`]).
    login_service: bool,
    /// The search for set-id files of the guest on every file system that
    /// allows them: at `reclogind`'s start only.
    setid_walk: bool,
}

/// Steps 2 to 7 of the routine; the caller holds the guest lock (step 1).
/// Each step is safe to repeat. Sets or clears the failure mark.
fn cleanup_locked(_held: &Lock, steps: Steps) -> Outcome {
    let mut left = Vec::new();
    let deadline = Instant::now() + WALK_BUDGET;
    // Scheduled jobs first, so that nothing starts a process of the guest's
    // after the sweep.
    for path in CRONTABS {
        if let Err(e) = remove_path(path, deadline) {
            log(WHO, format_args!("removing {path}: {e}"));
        }
    }
    for spool in AT_SPOOLS {
        let walk = shared_place(spool, Mode::Remove, deadline);
        if walk.failed > 0 {
            log(
                WHO,
                format_args!(
                    "{} of the guest's jobs under {spool} not removed",
                    walk.failed
                ),
            );
        }
    }
    // 2. The login service, then the uid sweep (which also clears the
    // guest's keyrings).
    if steps.login_service {
        ask_login_service_to_end_user(UID);
    }
    if let Err(why) = sweep() {
        log(WHO, format_args!("the sweep: {why}"));
        left.push(format!("the sweep: {why}"));
    }
    // 3. Everything of the guest's mounted, then the folders.
    unmount_all();
    // 3 and 5. The folders and the named leftovers.
    for path in LEFTOVERS {
        if let Err(e) = remove_path(path, deadline) {
            log(WHO, format_args!("removing {path}: {e}"));
        }
    }
    // 4. The guest's files in the shared writable places.
    for place in SHARED_PLACES {
        let walk = shared_place(place, Mode::Remove, deadline);
        if walk.failed > 0 {
            log(
                WHO,
                format_args!(
                    "{} of the guest's files under {place} not removed",
                    walk.failed
                ),
            );
        }
    }
    // The system's records that uid 60600 could read.
    let records = system_records(true);
    if records.failed > 0 {
        log(
            WHO,
            format_args!(
                "{} journal files or core dumps of the guest not removed",
                records.failed
            ),
        );
    }
    // 6. System V shared memory, semaphores, message queues.
    remove_ipc();
    // Set-id files of the guest anywhere (at start only).
    let setid = steps.setid_walk.then(strip_setid_everywhere);
    // 7. The check.
    left.extend(check());
    match setid {
        Some(Ok(stripped)) => {
            if stripped > 0 {
                log(
                    WHO,
                    format_args!("set-id bits taken from {stripped} files of uid {UID}"),
                );
            }
            set_mark(SETID_CHECKED_MARK, true);
        }
        Some(Err(why)) => left.push(why),
        None => {}
    }
    let failed = !left.is_empty();
    set_mark(FAILED_MARK, failed);
    if failed {
        Outcome::Failed(left.join("; "))
    } else {
        Outcome::Clean
    }
}

/// Create (`on`) or remove a mark file in `/run/reclogin`.
fn set_mark(path: &str, on: bool) {
    if on {
        if let Err(e) = ensure_run_dir().and_then(|()| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map(drop)
        }) {
            log(WHO, format_args!("writing {path}: {e}"));
        }
    } else if let Err(e) = fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            log(WHO, format_args!("removing {path}: {e}"));
        }
    }
}

/// Step 7: what is left. Empty means finished completely.
fn check() -> Vec<String> {
    let deadline = Instant::now() + WALK_BUDGET;
    let mut left = Vec::new();
    let processes = guest_processes();
    if processes > 0 {
        left.push(format!("{processes} processes of uid {UID}"));
    }
    match mountinfo() {
        Ok(mounts) => {
            let points: Vec<String> = mounts
                .iter()
                .filter(|m| is_guests_mount(m) || is_guest_fuse(m))
                .map(|m| String::from_utf8_lossy(&m.point).into_owned())
                .collect();
            if !points.is_empty() {
                left.push(format!("still mounted: {}", points.join(", ")));
            }
        }
        Err(e) => left.push(format!("the mounts cannot be read: {e}")),
    }
    for path in LEFTOVERS.iter().chain(CRONTABS.iter()) {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => left.push(format!("still there: {path}")),
            Err(e) => left.push(format!("{path} cannot be looked at: {e}")),
        }
    }
    for place in SHARED_PLACES.iter().chain(AT_SPOOLS.iter()) {
        let walk = shared_place(place, Mode::Count, deadline);
        if walk.timed_out {
            left.push(format!("the check of {place} took too long"));
        } else if walk.found > 0 || walk.failed > 0 {
            left.push(format!(
                "{} of the guest's files under {place} ({} could not be looked at)",
                walk.found, walk.failed
            ));
        }
    }
    let records = system_records(false);
    if records.found > 0 || records.failed > 0 {
        left.push(format!(
            "{} journal files or core dumps of uid {UID}",
            records.found + records.failed
        ));
    }
    let ipc = ipc_objects(true).len();
    if ipc > 0 {
        left.push(format!("{ipc} System V objects of uid {UID}"));
    }
    left
}

// ---------------------------------------------------------------------------
// the cleanup child: `reclogind --guest-cleanup`

/// The cleanup child's life: the guest lock it was handed as descriptor 3,
/// the whole routine with the login service (and with `--setid-walk` the
/// search for set-id files), then exit: 0 clean, 1 failed, 2 not run.
pub fn cleanup_child() -> ExitCode {
    if unsafe { libc::geteuid() } != 0 {
        log(WHO, format_args!("{CLEANUP_SWITCH} must run as root"));
        return ExitCode::from(2);
    }
    let setid_walk = std::env::args().any(|a| a == SETID_WALK_ARG);
    let lock = match inherited_lock() {
        Ok(lock) => lock,
        Err(e) => {
            log(
                WHO,
                format_args!("the guest lock as descriptor {LOCK_FD}: {e}"),
            );
            return ExitCode::from(2);
        }
    };
    let outcome = cleanup_locked(
        &lock,
        Steps {
            login_service: true,
            setid_walk,
        },
    );
    report("in the cleanup process", &outcome);
    match outcome {
        Outcome::Clean => ExitCode::SUCCESS,
        Outcome::Failed(_) => ExitCode::FAILURE,
    }
}

/// Descriptor 3, checked to be the guest lock and held by this process's
/// open file (taking a lock already held through the same open file
/// succeeds; one held by someone else does not).
fn inherited_lock() -> io::Result<Lock> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(LOCK_FD, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(LOCK_FD) };
    if unsafe { libc::fcntl(LOCK_FD, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = fs::symlink_metadata(LOCK_PATH)?;
    if st.st_dev != path.dev() || st.st_ino != path.ino() {
        return Err(io::Error::other(format!("it is not {LOCK_PATH}")));
    }
    if unsafe { libc::flock(LOCK_FD, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Lock(file))
}

/// The cleanup child as `reclogind` keeps it.
#[derive(Default)]
pub struct Cleaner {
    child: Option<Child>,
    /// Asked for again while one ran: another run follows.
    again: bool,
    /// When a failed or skipped cleanup is tried again.
    retry_at: Option<Instant>,
    back_off: Duration,
}

impl Cleaner {
    fn schedule_retry(&mut self) {
        let wait = if self.back_off.is_zero() {
            RETRY_FIRST
        } else {
            self.back_off
        };
        self.back_off = (wait * 2).min(RETRY_MOST);
        self.retry_at = Some(Instant::now() + wait);
        log(
            WHO,
            format_args!("the guest's cleanup runs again in {} s", wait.as_secs()),
        );
    }
}

impl Daemon {
    /// Run the cleanup in a child process (at start, after every guest
    /// worker, and again after a failure). `reclogind` takes the guest lock
    /// and hands it to the child; while the child runs the screen does not
    /// offer Guest. When the lock is held (a guest session runs, or a
    /// cleanup) nothing is started and a later try is planned.
    pub(crate) fn start_guest_cleanup(&mut self, why: &str) {
        if !account_ok() {
            return;
        }
        if self.login.cleaner.child.is_some() {
            self.login.cleaner.again = true;
            return;
        }
        let lock = match claim(Role::Cleanup) {
            Ok(Claim::Got(lock)) => lock,
            Ok(Claim::Held(holder)) => {
                log(
                    WHO,
                    format_args!("cleanup {why}: not started, the guest lock is held ({holder:?})"),
                );
                self.login.cleaner.schedule_retry();
                return;
            }
            Err(e) => {
                log(
                    WHO,
                    format_args!("cleanup {why}: the guest lock {LOCK_PATH}: {e}"),
                );
                self.login.cleaner.schedule_retry();
                return;
            }
        };
        let fd = lock.0.as_raw_fd();
        let mut command = Command::new("/proc/self/exe");
        command.arg(CLEANUP_SWITCH);
        if !setid_checked() {
            command.arg(SETID_WALK_ARG);
        }
        command
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .stdin(Stdio::null())
            .current_dir("/");
        unsafe {
            command.pre_exec(move || {
                // Only async-signal-safe calls between fork and exec.
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                if fd == LOCK_FD {
                    if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(fd, LOCK_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        match command.spawn() {
            Ok(child) => {
                let _ = write_holder(&lock.0, Role::Cleanup, child.id() as i32);
                log(
                    WHO,
                    format_args!("cleanup {why}: started (process {})", child.id()),
                );
                self.login.cleaner.child = Some(child);
                self.login.cleaner.retry_at = None;
                CLEANING.store(true, Ordering::Relaxed);
                // The child holds the lock now through its own descriptor.
                drop(lock);
                let _ = self.send_guest_state();
            }
            Err(e) => {
                log(WHO, format_args!("cleanup {why}: starting it: {e}"));
                drop(lock);
                self.login.cleaner.schedule_retry();
            }
        }
    }

    /// From the main loop (each turn and on `SIGCHLD`): a cleanup child that
    /// ended is reaped and the screen gets a fresh state either way; a
    /// failed cleanup is tried again when its pause is over.
    pub(crate) fn reap_guest_cleanup(&mut self) -> Next {
        if let Some(child) = self.login.cleaner.child.as_mut() {
            let status = match child.try_wait() {
                Ok(None) => return Next::Continue,
                Ok(Some(status)) => Some(status),
                Err(_) => None,
            };
            self.login.cleaner.child = None;
            CLEANING.store(false, Ordering::Relaxed);
            let clean = status.is_some_and(|s| s.success());
            if clean {
                self.login.cleaner.back_off = Duration::ZERO;
            } else {
                log(
                    WHO,
                    format_args!("the cleanup process ended: {status:?}; the guest is not offered"),
                );
                self.login.cleaner.schedule_retry();
            }
            if std::mem::take(&mut self.login.cleaner.again) {
                self.start_guest_cleanup("asked for again");
            }
            return self.send_guest_state();
        }
        if self
            .login
            .cleaner
            .retry_at
            .is_some_and(|t| Instant::now() >= t)
        {
            self.login.cleaner.retry_at = None;
            if cleanup_failed() || !setid_checked() {
                self.start_guest_cleanup("after a pause");
            }
        }
        Next::Continue
    }

    /// A fresh state for the greeter (whether Guest is offered changed),
    /// when one runs and has its display.
    fn send_guest_state(&mut self) -> Next {
        if self.login.greeter_away() || self.supervisor.handshake_pending {
            return Next::Continue;
        }
        let state = self.supervisor.state();
        self.send(&ToGreeter::State(Box::new(state)))
    }

    /// `end_guest` from the greeter: the running guest session is asked to
    /// end. Answered with `auth_result`: `ok` when it was asked, else why
    /// not. The screen follows the login service's next report.
    pub(crate) fn on_end_guest(&mut self) -> Next {
        if end_running_session() {
            self.logind.refresh();
            self.send(&ToGreeter::AuthResult {
                ok: true,
                text: String::new(),
            })
        } else {
            self.send(&ToGreeter::AuthResult {
                ok: false,
                text: "No guest session is running.".into(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// processes (design point 3, "Ending the guest's processes")

/// The real, effective, saved and file-system uids of process `pid`, and
/// whether it is a zombie.
fn ids_of(pid: &str) -> Option<(Vec<u32>, bool)> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let (mut ids, mut dead) = (Vec::new(), false);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            dead = matches!(rest.trim_start().chars().next(), Some('Z' | 'X'));
        } else if let Some(rest) = line.strip_prefix("Uid:") {
            ids = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
        }
    }
    Some((ids, dead))
}

/// The numbers of the live processes in `/proc` that `pick` chooses by
/// their uids; `None` when `/proc` cannot be read.
fn processes_where(pick: impl Fn(&[u32]) -> bool) -> Option<Vec<i32>> {
    let dir = fs::read_dir("/proc").ok()?;
    Some(
        dir.flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let pid: i32 = name.parse().ok()?;
                let (ids, dead) = ids_of(name)?;
                (!dead && pick(&ids)).then_some(pid)
            })
            .collect(),
    )
}

/// How many processes have the real or saved uid 60600 (the ones the sweep
/// can signal), zombies not counted. An unreadable `/proc` counts as one, so
/// that the check fails rather than passes blind.
fn guest_processes() -> usize {
    processes_where(|ids| ids.first() == Some(&UID) || ids.get(2) == Some(&UID))
        .map_or(1, |p| p.len())
}

/// The sweep helper's account: `reclogin-sweep`, exactly uid 60601, and no
/// other account with that number.
fn sweeper() -> Result<Account, String> {
    let account =
        Account::lookup(SWEEP_NAME).map_err(|e| format!("no account {SWEEP_NAME}: {e}"))?;
    if account.uid != SWEEP_UID {
        return Err(format!(
            "the account {SWEEP_NAME} has uid {}, not {SWEEP_UID}",
            account.uid
        ));
    }
    let others = others_with_uid(SWEEP_UID, SWEEP_NAME)?;
    if !others.is_empty() {
        return Err(format!(
            "uid {SWEEP_UID} also belongs to {}",
            others.join(", ")
        ));
    }
    Ok(account)
}

/// The uid sweep: a helper clears the guest's keyrings and sends `SIGKILL`
/// to every process of the guest, wherever it hides; repeated until `/proc`
/// shows none, for up to ten seconds. It runs at least once (the keyrings),
/// and gives up after [`SWEEP_TRIES`] helpers that did not finish.
fn sweep() -> Result<(), String> {
    let sweeper = sweeper()?;
    let deadline = Instant::now() + SWEEP_FOR;
    let mut ran = false;
    let mut failures = 0;
    loop {
        if ran && guest_processes() == 0 {
            return Ok(());
        }
        match kill_as_guest(&sweeper) {
            Ok(()) => ran = true,
            Err(why) => {
                failures += 1;
                log(WHO, format_args!("a sweep helper failed: {why}"));
                if failures >= SWEEP_TRIES {
                    return Err(format!("{failures} sweep helpers failed; the last: {why}"));
                }
            }
        }
        if Instant::now() >= deadline {
            let left = guest_processes();
            return if ran && left == 0 {
                Ok(())
            } else {
                Err(format!(
                    "{left} processes of uid {UID} after {} seconds",
                    SWEEP_FOR.as_secs()
                ))
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const KEYCTL_CLEAR: libc::c_long = 7;
const KEY_SPEC_USER_KEYRING: libc::c_long = -4;
const KEY_SPEC_USER_SESSION_KEYRING: libc::c_long = -5;

/// One sweep helper. Forked; in the child only system calls and reads of
/// `/proc` (the caller is single-threaded).
///
/// The ids, in order:
///
/// 1. groups none, real, effective and saved gid `reclogin-sweep`'s;
/// 2. real and effective uid 60600, saved uid 60601: with no id 0 left the
///    kernel drops every capability (so no `CAP_KILL`), and with real uid
///    60600 `KEY_SPEC_USER_KEYRING` and `KEY_SPEC_USER_SESSION_KEYRING` name
///    the guest's keyrings (the kernel looks them up by the real uid), which
///    are cleared;
/// 3. real uid 60601 (allowed without rights: it is the saved uid),
///    effective uid 60600, saved uid 60601, checked;
/// 4. no other process may have uid 60601 in any slot;
/// 5. `kill(-1, SIGKILL)`.
///
/// The kernel's rule for `kill` (`kill_ok_by_cred` in kernel/signal.c): a
/// process without `CAP_KILL` may signal a target when the sender's real or
/// effective uid equals the target's real or saved uid. `kill(-1)` sends to
/// every such process except pid 1 and the sender itself. The helper's real
/// uid is 60601 and its effective uid 60600, so it reaches exactly the
/// processes whose real or saved uid is 60600 (the guest's, as only the
/// guest has that uid) or 60601 (nobody: step 4). A guest process has real,
/// effective and saved uid 60600 and can only move among those; the helper's
/// real and saved uid are both 60601, so the guest cannot signal it, nor
/// trace it (tracing needs all three of the helper's uids to match, and
/// the helper is not dumpable after changing ids). Only in step 2, while
/// its real uid is 60600, can a guest process signal it: a helper stopped
/// or killed there is killed by its parent after [`SWEEP_HELPER_WAIT`] and
/// another helper runs.
fn kill_as_guest(sweeper: &Account) -> Result<(), String> {
    let sweep_gid = sweeper.gid;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            );
            let (mut r, mut e, mut s) = (0, 0, 0);
            if libc::syscall(libc::SYS_setgroups, 0usize, std::ptr::null::<libc::gid_t>()) != 0
                || libc::syscall(libc::SYS_setresgid, sweep_gid, sweep_gid, sweep_gid) != 0
                || libc::syscall(libc::SYS_setresuid, UID, UID, SWEEP_UID) != 0
                || libc::getresuid(&mut r, &mut e, &mut s) != 0
                || (r, e, s) != (UID, UID, SWEEP_UID)
                || libc::getgroups(0, std::ptr::null_mut()) != 0
            {
                libc::_exit(1);
            }
            // The guest's keyrings: their keys survive the session otherwise.
            libc::syscall(libc::SYS_keyctl, KEYCTL_CLEAR, KEY_SPEC_USER_KEYRING);
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_CLEAR,
                KEY_SPEC_USER_SESSION_KEYRING,
            );
            if libc::syscall(libc::SYS_setresuid, SWEEP_UID, UID, SWEEP_UID) != 0
                || libc::getresuid(&mut r, &mut e, &mut s) != 0
                || (r, e, s) != (SWEEP_UID, UID, SWEEP_UID)
            {
                libc::_exit(1);
            }
            let me = libc::getpid();
            match processes_where(|ids| ids.contains(&SWEEP_UID)) {
                Some(found) if found.iter().all(|&p| p == me) => {}
                _ => libc::_exit(3),
            }
            libc::kill(-1, libc::SIGKILL);
            libc::_exit(0);
        }
    }
    let deadline = Instant::now() + SWEEP_HELPER_WAIT;
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid {
            break;
        }
        if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(format!("waiting: {}", io::Error::last_os_error()));
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return Err(format!(
                "it did not finish within {} seconds",
                SWEEP_HELPER_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => Ok(()),
            3 => Err(format!("another process has uid {SWEEP_UID}")),
            _ => Err("it could not take its ids".into()),
        }
    } else {
        Err(format!("it ended on signal {}", libc::WTERMSIG(status)))
    }
}

// ---------------------------------------------------------------------------
// mounts

/// One line of `/proc/self/mountinfo`.
struct Mount {
    id: u64,
    /// The mount point, unescaped.
    point: Vec<u8>,
    /// The per-mount options (`rw`, `nosuid`, …).
    options: String,
    fstype: String,
    /// The file system's own options (FUSE's `user_id=`).
    super_options: String,
}

fn mountinfo() -> io::Result<Vec<Mount>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let sep = fields.iter().position(|f| *f == "-")?;
            Some(Mount {
                id: fields.first()?.parse().ok()?,
                point: unescape(fields.get(4)?),
                options: fields.get(5)?.to_string(),
                fstype: fields.get(sep + 1)?.to_string(),
                super_options: fields.get(sep + 3).unwrap_or(&"").to_string(),
            })
        })
        .collect())
}

/// mountinfo writes space, tab, newline and backslash as `\ooo`.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            if let Some(value) = field
                .get(i + 1..i + 4)
                .and_then(|digits| u8::from_str_radix(digits, 8).ok())
            {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Whether `point` is `root` or under it.
fn at_or_under(point: &[u8], root: &str) -> bool {
    let root = root.as_bytes();
    point == root
        || point
            .strip_prefix(root)
            .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// A FUSE mount the guest made (FUSE writes the mounting user's uid into
/// the options).
fn is_guest_fuse(m: &Mount) -> bool {
    m.fstype.starts_with("fuse")
        && m.super_options
            .split(',')
            .any(|o| o == format!("user_id={UID}"))
}

/// A mount the cleanup takes away: anything at or under the guest's own
/// places, or a FUSE mount of the guest's under a shared place. Nothing
/// else is ever unmounted.
fn is_guests_mount(m: &Mount) -> bool {
    MOUNTED_UNDER.iter().any(|root| at_or_under(&m.point, root))
        || (is_guest_fuse(m) && SHARED_PLACES.iter().any(|root| at_or_under(&m.point, root)))
}

/// Open `path` as a handle (`O_PATH`), one name at a time from `/`, never
/// following a symbolic link, so nobody can steer it elsewhere by swapping
/// a folder for a link.
fn open_path_nofollow(path: &[u8]) -> io::Result<OwnedFd> {
    let mut fd = {
        let raw = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { OwnedFd::from_raw_fd(raw) }
    };
    for name in path.split(|&b| b == b'/').filter(|n| !n.is_empty()) {
        if name == b"." || name == b".." {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let c_name = CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
        let raw = unsafe {
            libc::openat(
                fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        fd = unsafe { OwnedFd::from_raw_fd(raw) };
    }
    Ok(fd)
}

/// The mount id of the mount an open handle is on (`/proc/self/fdinfo`).
fn mount_id_of(fd: RawFd) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Take away everything of the guest's mounted, deepest first. Each mount
/// is opened from `/` without following links, checked to be the very
/// mount (its id) chosen from mountinfo, and detached through
/// `/proc/self/fd/N`, which names the opened mount itself, not a path that
/// could be looked up again. Detaching (lazy unmount) is always used: the
/// handle itself keeps the mount busy. It is unreachable at once and freed
/// when the last reference goes (design point 2, "A wipe that did not
/// finish").
fn unmount_all() {
    for _ in 0..8 {
        let mounts = match mountinfo() {
            Ok(mounts) => mounts,
            Err(e) => {
                log(WHO, format_args!("reading the mounts: {e}"));
                return;
            }
        };
        let mut ours: Vec<&Mount> = mounts.iter().filter(|m| is_guests_mount(m)).collect();
        if ours.is_empty() {
            return;
        }
        ours.sort_by_key(|m| std::cmp::Reverse(m.point.len()));
        for m in ours {
            let shown = String::from_utf8_lossy(&m.point).into_owned();
            let fd = match open_path_nofollow(&m.point) {
                Ok(fd) => fd,
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(e) => {
                    log(WHO, format_args!("opening the mount {shown}: {e}"));
                    continue;
                }
            };
            if mount_id_of(fd.as_raw_fd()) != Some(m.id) {
                // Gone meanwhile, or something else is there now.
                continue;
            }
            let Ok(proc_path) = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())) else {
                continue;
            };
            if unsafe { libc::umount2(proc_path.as_ptr(), libc::MNT_DETACH) } == 0 {
                log(WHO, format_args!("{shown} detached"));
            } else {
                let error = io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) {
                    log(WHO, format_args!("detaching {shown}: {error}"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// files: every walk works from open folder handles (`openat`, `fstatat`,
// `unlinkat`), never follows a symbolic link, never leaves the file system
// it is on for another mount, and has no depth limit: it keeps only the
// handle of the folder it is in and climbs back with `..`, checked to be the
// folder it came from (by device and inode); when it is not (a folder was
// moved meanwhile), the walk begins again from its top.

fn open_dir_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A folder opened for reading by its path, one name at a time from `/`
/// without following links.
fn open_dir(path: &[u8]) -> io::Result<OwnedFd> {
    let handle = open_path_nofollow(path)?;
    open_dir_at(handle.as_raw_fd(), c".")
}

fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn is_reg(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

fn errno_is(rc: libc::c_int, code: libc::c_int) -> bool {
    rc != 0 && io::Error::last_os_error().raw_os_error() == Some(code)
}

/// The names in an open folder, without `.` and `..`.
fn entries(dir: RawFd) -> io::Result<Vec<CString>> {
    let copy = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(copy) };
        return Err(error);
    }
    // fdopendir reads from the descriptor's offset; a fresh walk starts at 0.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// What a walk does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Remove what is the guest's (a folder of the guest's with all it holds).
    Remove,
    /// Count what is the guest's.
    Count,
    /// Take the set-id bits from files and folders of uid 60600.
    StripSetid,
}

#[derive(Default)]
struct Walk {
    /// Things of the guest's seen (with `Remove`, also the ones removed;
    /// with `StripSetid`, the set-id ones).
    found: usize,
    /// Things that could not be removed, fixed or looked at.
    failed: usize,
    /// The budget ran out (counted in `failed` too).
    timed_out: bool,
}

/// One folder on the way down.
struct Frame {
    dev: libc::dev_t,
    ino: libc::ino_t,
    /// Its name in the folder above (`None` for the top).
    name: Option<CString>,
    path: Vec<u8>,
    /// Everything in it is the guest's (`Remove`: it is removed on the way
    /// back up).
    all: bool,
    names: Vec<CString>,
    next: usize,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Walk the tree under the open folder `top` (whose path is `top_path`).
/// `top_all`: everything in it is the guest's. `mounts`: every mount point
/// on the system; one is never entered (another walk or nobody's business),
/// and an entry that cannot even be looked at is skipped only when it is
/// one (someone's mount that root may not look into). Every other error
/// but "does not exist" counts as failed.
fn walk_tree(
    top: RawFd,
    top_path: &[u8],
    top_all: bool,
    mode: Mode,
    mounts: &HashSet<Vec<u8>>,
    deadline: Instant,
) -> Walk {
    let mut walk = Walk::default();
    let mut restarts = 0;
    'again: loop {
        walk.found = 0;
        walk.failed = 0;
        let (Ok(top_st), Ok(names)) = (fstat(top), entries(top)) else {
            walk.failed += 1;
            return walk;
        };
        let first = unsafe { libc::fcntl(top, libc::F_DUPFD_CLOEXEC, 0) };
        if first < 0 {
            walk.failed += 1;
            return walk;
        }
        let mut cur = unsafe { OwnedFd::from_raw_fd(first) };
        let mut stack = vec![Frame {
            dev: top_st.st_dev,
            ino: top_st.st_ino,
            name: None,
            path: top_path.to_vec(),
            all: top_all,
            names,
            next: 0,
        }];
        loop {
            if Instant::now() >= deadline {
                walk.timed_out = true;
                walk.failed += 1;
                return walk;
            }
            let frame = stack.last_mut().expect("never empty here");
            if frame.next < frame.names.len() {
                let name = frame.names[frame.next].clone();
                frame.next += 1;
                let (dev, all) = (frame.dev, frame.all);
                let path = join(&frame.path, name.to_bytes());
                let st = match stat_at(cur.as_raw_fd(), &name) {
                    Ok(st) => st,
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                    Err(_) if mounts.contains(&path) => continue,
                    Err(_) => {
                        walk.found += 1;
                        walk.failed += 1;
                        continue;
                    }
                };
                let ours = all || st.st_uid == UID;
                let dir = is_dir(&st);
                if st.st_dev != dev && mounts.contains(&path) {
                    // Another file system mounted here: not entered. One of
                    // the guest's that is still there fails the check.
                    if ours && mode != Mode::StripSetid {
                        walk.found += 1;
                        walk.failed += 1;
                    }
                    continue;
                }
                match mode {
                    Mode::Count => {
                        if ours {
                            walk.found += 1;
                            continue;
                        }
                    }
                    Mode::Remove => {
                        if ours && !dir {
                            walk.found += 1;
                            let rc = unsafe { libc::unlinkat(cur.as_raw_fd(), name.as_ptr(), 0) };
                            if rc != 0 && !errno_is(rc, libc::ENOENT) {
                                walk.failed += 1;
                            }
                        }
                    }
                    Mode::StripSetid => {
                        if st.st_uid == UID && st.st_mode & 0o6000 != 0 && (dir || is_reg(&st)) {
                            walk.found += 1;
                            if let Err(e) = strip_setid(cur.as_raw_fd(), &name, &st) {
                                log(
                                    WHO,
                                    format_args!(
                                        "taking the set-id bits from {}: {e}",
                                        String::from_utf8_lossy(&path)
                                    ),
                                );
                                walk.failed += 1;
                            }
                        }
                    }
                }
                if !dir {
                    continue;
                }
                // A folder: into it.
                let child = match open_dir_at(cur.as_raw_fd(), &name) {
                    Ok(fd) => fd,
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                    Err(_) => {
                        walk.failed += 1;
                        continue;
                    }
                };
                let names = match (fstat(child.as_raw_fd()), entries(child.as_raw_fd())) {
                    (Ok(cst), Ok(names)) if cst.st_dev == st.st_dev && cst.st_ino == st.st_ino => {
                        names
                    }
                    // Swapped meanwhile, or unreadable.
                    _ => {
                        walk.failed += 1;
                        continue;
                    }
                };
                stack.push(Frame {
                    dev: st.st_dev,
                    ino: st.st_ino,
                    name: Some(name),
                    path,
                    all: ours && mode == Mode::Remove,
                    names,
                    next: 0,
                });
                cur = child;
            } else {
                let done = stack.pop().expect("never empty here");
                let Some(parent) = stack.last() else {
                    return walk;
                };
                let up = open_dir_at(cur.as_raw_fd(), c"..");
                let same = up
                    .as_ref()
                    .ok()
                    .and_then(|fd| fstat(fd.as_raw_fd()).ok())
                    .is_some_and(|p| p.st_dev == parent.dev && p.st_ino == parent.ino);
                let Ok(up) = up
                    .map_err(drop)
                    .and_then(|fd| if same { Ok(fd) } else { Err(()) })
                else {
                    restarts += 1;
                    if restarts > MAX_RESTARTS {
                        walk.failed += 1;
                        return walk;
                    }
                    continue 'again;
                };
                cur = up;
                if done.all {
                    // A folder of the guest's, emptied: removed itself.
                    walk.found += 1;
                    if let Some(name) = &done.name {
                        let rc = unsafe {
                            libc::unlinkat(cur.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                        };
                        if rc != 0 && !errno_is(rc, libc::ENOENT) {
                            walk.failed += 1;
                        }
                    }
                }
            }
        }
    }
}

/// Every mount point on the system (for [`walk_tree`]); empty when
/// mountinfo cannot be read.
fn mount_points() -> HashSet<Vec<u8>> {
    mountinfo()
        .map(|mounts| mounts.into_iter().map(|m| m.point).collect())
        .unwrap_or_default()
}

/// Step 4 (and the `at` spools) for one place: the guest's files found and,
/// with `Remove`, removed. A place that does not exist has none; one that
/// cannot be opened for another reason fails.
fn shared_place(place: &str, mode: Mode, deadline: Instant) -> Walk {
    let dir = match open_dir(place.as_bytes()) {
        Ok(dir) => dir,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Walk::default(),
        Err(_) => {
            return Walk {
                failed: 1,
                ..Walk::default()
            }
        }
    };
    walk_tree(
        dir.as_raw_fd(),
        place.as_bytes(),
        false,
        mode,
        &mount_points(),
        deadline,
    )
}

/// Remove `path` whole (a folder with everything in it), without following
/// a link anywhere on the way. Missing is fine.
fn remove_path(path: &str, deadline: Instant) -> io::Result<()> {
    let path = Path::new(path);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let parent = match open_dir(parent.as_os_str().as_bytes()) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let name = CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let st = match stat_at(parent.as_raw_fd(), &name) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let flag = if is_dir(&st) {
        let dir = open_dir_at(parent.as_raw_fd(), &name)?;
        let walk = walk_tree(
            dir.as_raw_fd(),
            path.as_os_str().as_bytes(),
            true,
            Mode::Remove,
            &mount_points(),
            deadline,
        );
        if walk.failed > 0 {
            return Err(io::Error::other(format!(
                "{} things inside could not be removed",
                walk.failed
            )));
        }
        libc::AT_REMOVEDIR
    } else {
        0
    };
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flag) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// set-id files of the guest (finding 1 of the review; the backstop behind
// `nosetid.rs`)

/// File systems never searched: the kernel's own and network ones (FUSE
/// mounts of users are `nosuid` by force).
const NOT_SEARCHED: [&str; 31] = [
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "securityfs",
    "debugfs",
    "tracefs",
    "pstore",
    "efivarfs",
    "bpf",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ceph",
    "glusterfs",
    "9p",
    "afs",
    "lustre",
    "ncpfs",
];

/// Whether a set-id file could be made and used on this mount: written to
/// (`rw`) and set-id allowed (no `nosuid`), and a local, real file system.
fn may_hold_setid(m: &Mount) -> bool {
    let options: Vec<&str> = m.options.split(',').collect();
    let read_only = options.contains(&"ro") || m.super_options.split(',').any(|o| o == "ro");
    !options.contains(&"nosuid")
        && !read_only
        && !m.fstype.starts_with("fuse")
        && !NOT_SEARCHED.contains(&m.fstype.as_str())
}

/// Search every mounted local file system that is written to and allows
/// set-id programs for files and folders of uid 60600 with a set-id bit, and
/// take the bits away. Each file system is walked once, from its mount
/// point, staying on it; the whole search may take [`SETID_BUDGET`], and one
/// that runs longer fails the check. The number of files fixed, or why the
/// search did not finish.
///
/// It runs only at `reclogind`'s start (in the cleanup child, so the screen
/// is not held up; Guest is offered only after it finished clean). It walks
/// every home and all of `/var`, which on a big disk takes too long to do
/// before each guest; and inside a session the kernel filter (`nosetid.rs`)
/// already refuses to make such a file, so this search is the backstop for
/// files made outside it (by an older reclogin, or a process the filter did
/// not cover), found once per boot.
fn strip_setid_everywhere() -> Result<usize, String> {
    let deadline = Instant::now() + SETID_BUDGET;
    let mounts = mountinfo().map_err(|e| format!("the mounts cannot be read: {e}"))?;
    let points: HashSet<Vec<u8>> = mounts.iter().map(|m| m.point.clone()).collect();
    let mut seen = HashSet::new();
    let (mut found, mut failed, mut timed_out) = (0, 0, false);
    for m in mounts.iter().filter(|m| may_hold_setid(m)) {
        let dir = match open_dir(&m.point) {
            Ok(dir) => dir,
            // Hidden under another mount, or gone: nobody reaches it by path.
            Err(_) => continue,
        };
        if mount_id_of(dir.as_raw_fd()) != Some(m.id) {
            continue;
        }
        let Ok(st) = fstat(dir.as_raw_fd()) else {
            failed += 1;
            continue;
        };
        if !seen.insert((st.st_dev, st.st_ino)) {
            continue;
        }
        let walk = walk_tree(
            dir.as_raw_fd(),
            &m.point,
            false,
            Mode::StripSetid,
            &points,
            deadline,
        );
        found += walk.found;
        failed += walk.failed;
        if walk.timed_out {
            timed_out = true;
            break;
        }
    }
    if timed_out {
        Err(format!(
            "the search for set-id files of uid {UID} took over {} seconds",
            SETID_BUDGET.as_secs()
        ))
    } else if failed > 0 {
        Err(format!(
            "the search for set-id files of uid {UID} could not look at or fix {failed} things"
        ))
    } else {
        Ok(found)
    }
}

/// Take the set-id bits from `name` in `dir`: opened as a handle without
/// following a link, checked again to be the same file of uid 60600, and
/// changed through `/proc/self/fd/N`, so that a file swapped in meanwhile is
/// never touched.
fn strip_setid(dir: RawFd, name: &CStr, seen: &libc::stat) -> io::Result<()> {
    let raw = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let st = fstat(fd.as_raw_fd())?;
    if st.st_dev != seen.st_dev
        || st.st_ino != seen.st_ino
        || st.st_uid != UID
        || !(is_reg(&st) || is_dir(&st))
    {
        return Err(io::Error::other("it changed meanwhile"));
    }
    if st.st_mode & 0o6000 == 0 {
        return Ok(());
    }
    let proc_path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    if unsafe { libc::chmod(proc_path.as_ptr(), st.st_mode & 0o1777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the system's records of the guest

/// A per-user journal file of uid 60600.
fn is_guest_journal(name: &[u8]) -> bool {
    let prefix = format!("user-{UID}");
    name.strip_prefix(prefix.as_bytes())
        .is_some_and(|rest| rest.starts_with(b".journal") || rest.starts_with(b"@"))
}

/// A systemd-coredump file of uid 60600: `core.<command>.<uid>.<boot id>…`,
/// the uid being the field before a 32-digit boot id (the command may hold
/// dots itself).
fn is_guest_coredump(name: &[u8]) -> bool {
    let Some(rest) = name.strip_prefix(b"core.") else {
        return false;
    };
    let fields: Vec<&[u8]> = rest.split(|&b| b == b'.').collect();
    let number = UID.to_string();
    fields.windows(2).any(|pair| {
        pair[0] == number.as_bytes()
            && pair[1].len() == 32
            && pair[1].iter().all(u8::is_ascii_hexdigit)
    })
}

/// The guest's journal files (in each folder under [`JOURNAL_DIRS`]) and
/// core dumps ([`COREDUMP_DIR`]): counted, and with `delete` removed. A
/// folder that does not exist has none.
fn system_records(delete: bool) -> Walk {
    let mut walk = Walk::default();
    let mut sweep_dir = |dir: RawFd, pick: &dyn Fn(&[u8]) -> bool| {
        let Ok(names) = entries(dir) else {
            walk.failed += 1;
            return;
        };
        for name in names.iter().filter(|n| pick(n.to_bytes())) {
            walk.found += 1;
            if delete {
                let rc = unsafe { libc::unlinkat(dir, name.as_ptr(), 0) };
                if rc != 0 && !errno_is(rc, libc::ENOENT) {
                    walk.failed += 1;
                }
            }
        }
    };
    for top in JOURNAL_DIRS {
        let Ok(dir) = open_dir(top.as_bytes()) else {
            continue;
        };
        // The machine's folders (`<machine id>`, `<machine id>.<namespace>`).
        let Ok(names) = entries(dir.as_raw_fd()) else {
            continue;
        };
        for name in names {
            if let Ok(machine) = open_dir_at(dir.as_raw_fd(), &name) {
                sweep_dir(machine.as_raw_fd(), &is_guest_journal);
            }
        }
    }
    if let Ok(dir) = open_dir(COREDUMP_DIR.as_bytes()) {
        sweep_dir(dir.as_raw_fd(), &is_guest_coredump);
    }
    walk
}

// ---------------------------------------------------------------------------
// System V objects (step 6)

#[derive(Debug, Clone, Copy)]
enum Ipc {
    Shm,
    Sem,
    Msg,
}

/// Objects whose owner or creator is uid 60600, from `/proc/sysvipc/`.
/// `skip_dying`: shared memory already marked for removal (it goes when
/// the last process detaches) is not listed.
fn ipc_objects(skip_dying: bool) -> Vec<(Ipc, i32)> {
    let mut out = Vec::new();
    for (kind, file, id_column) in [
        (Ipc::Shm, "shm", "shmid"),
        (Ipc::Sem, "sem", "semid"),
        (Ipc::Msg, "msg", "msqid"),
    ] {
        let Ok(text) = fs::read_to_string(format!("/proc/sysvipc/{file}")) else {
            continue;
        };
        let mut lines = text.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let columns: Vec<&str> = header.split_whitespace().collect();
        let at = |name: &str| columns.iter().position(|c| *c == name);
        let (Some(id), Some(uid), Some(cuid), Some(perms)) =
            (at(id_column), at("uid"), at("cuid"), at("perms"))
        else {
            continue;
        };
        for line in lines {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let number = |i: usize| fields.get(i).and_then(|v| v.parse::<u32>().ok());
            if number(uid) != Some(UID) && number(cuid) != Some(UID) {
                continue;
            }
            // SHM_DEST, in the mode as /proc prints it (octal).
            let dying = matches!(kind, Ipc::Shm)
                && fields
                    .get(perms)
                    .and_then(|p| u32::from_str_radix(p, 8).ok())
                    .is_some_and(|p| p & 0o1000 != 0);
            if skip_dying && dying {
                continue;
            }
            if let Some(Ok(n)) = fields.get(id).map(|v| v.parse::<i32>()) {
                out.push((kind, n));
            }
        }
    }
    out
}

fn remove_ipc() {
    for (kind, id) in ipc_objects(true) {
        let rc = unsafe {
            match kind {
                Ipc::Shm => libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()),
                Ipc::Sem => libc::semctl(id, 0, libc::IPC_RMID),
                Ipc::Msg => libc::msgctl(id, libc::IPC_RMID, std::ptr::null_mut()),
            }
        };
        if rc != 0 {
            log(
                WHO,
                format_args!("removing {kind:?} {id}: {}", io::Error::last_os_error()),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the memory home (design point 2)

/// `size=` for the home: a quarter of memory, at most 4 GiB; tmpfs's own
/// `25%` when the memory size cannot be read.
fn home_size() -> String {
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return "25%".into();
    }
    let total = (info.totalram as u64).saturating_mul(u64::from(info.mem_unit.max(1)));
    (total / 4).min(HOME_CAP).to_string()
}

/// Steps 1 to 4 of design point 2: the folders, a fresh tmpfs with the cap,
/// owner and mode, the check that it really is one and empty, and the
/// defaults copied in by a process that is the guest. The error is for the
/// log; the cleanup after the login undoes whatever was done. Runs after
/// the cleanup, when no process of the guest exists, on root's paths.
fn make_home() -> Result<(), String> {
    ensure_run_dir().map_err(|e| format!("{RUN_DIR}: {e}"))?;
    if unsafe { libc::mkdir(HOME_C.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(format!("making {HOME}: {error}"));
        }
    }
    let bare = fs::symlink_metadata(HOME).map_err(|e| format!("{HOME}: {e}"))?;
    if !bare.is_dir() || bare.uid() != 0 {
        return Err(format!("{HOME} is not a folder of root's"));
    }
    // Root's, mode 0000: a missing mount leaves nothing the guest can write.
    if unsafe { libc::chown(HOME_C.as_ptr(), 0, 0) } != 0
        || unsafe { libc::chmod(HOME_C.as_ptr(), 0) } != 0
    {
        return Err(format!("{HOME}: {}", io::Error::last_os_error()));
    }
    let run_dev = fs::symlink_metadata(RUN_DIR)
        .map_err(|e| format!("{RUN_DIR}: {e}"))?
        .dev();

    let options = CString::new(format!(
        "size={},mode=0700,uid={UID},gid={GID}",
        home_size()
    ))
    .map_err(|_| "mount options".to_string())?;
    let rc = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            HOME_C.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    };
    if rc != 0 {
        return Err(format!("mounting the home: {}", io::Error::last_os_error()));
    }

    // Really a fresh tmpfs of the guest's, and empty.
    let mut fs_info: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(HOME_C.as_ptr(), &mut fs_info) } != 0 {
        return Err(format!("{HOME}: {}", io::Error::last_os_error()));
    }
    #[allow(clippy::unnecessary_cast)]
    let fs_type = fs_info.f_type as u64;
    let home = fs::symlink_metadata(HOME).map_err(|e| format!("{HOME}: {e}"))?;
    let empty = fs::read_dir(HOME)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if fs_type != TMPFS_MAGIC
        || home.dev() == run_dev
        || home.uid() != UID
        || home.gid() != GID
        || home.mode() & 0o7777 != 0o700
        || !empty
    {
        return Err(format!(
            "{HOME} is not a fresh, empty tmpfs of the guest's after mounting"
        ));
    }
    copy_defaults_as_guest()
}

/// Copy the defaults trees into the home in a child process that has fully
/// become the guest (no groups, real, effective and saved uid and gid
/// 60600, checked), so root never works on a path inside the guest's home
/// with root's rights. Waited for at most [`COPY_WAIT`].
fn copy_defaults_as_guest() -> Result<(), String> {
    let sources: Vec<&str> = DEFAULTS
        .iter()
        .copied()
        .filter(|s| Path::new(s).is_dir())
        .collect();
    if sources.is_empty() {
        return Ok(());
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        // The worker is single-threaded: the child may use the library.
        let became = unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            );
            let (mut r, mut e, mut s) = (0, 0, 0);
            let (mut rg, mut eg, mut sg) = (0, 0, 0);
            libc::syscall(libc::SYS_setgroups, 0usize, std::ptr::null::<libc::gid_t>()) == 0
                && libc::syscall(libc::SYS_setresgid, GID, GID, GID) == 0
                && libc::syscall(libc::SYS_setresuid, UID, UID, UID) == 0
                && libc::getresuid(&mut r, &mut e, &mut s) == 0
                && libc::getresgid(&mut rg, &mut eg, &mut sg) == 0
                && (r, e, s) == (UID, UID, UID)
                && (rg, eg, sg) == (GID, GID, GID)
                && libc::getgroups(0, std::ptr::null_mut()) == 0
        };
        if !became {
            unsafe { libc::_exit(2) };
        }
        let mut ok = true;
        for source in sources {
            if let Err(e) = copy_tree(Path::new(source), Path::new(HOME)) {
                log(WHO, format_args!("copying {source}: {e}"));
                ok = false;
            }
        }
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let deadline = Instant::now() + COPY_WAIT;
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid {
            break;
        }
        if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(format!(
                "waiting for the copy: {}",
                io::Error::last_os_error()
            ));
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return Err(format!(
                "the copy of the defaults took over {} seconds",
                COPY_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    match (libc::WIFEXITED(status), libc::WEXITSTATUS(status)) {
        (true, 0) => Ok(()),
        (true, 2) => Err("the copy could not become the guest".into()),
        _ => Err("the defaults could not be copied".into()),
    }
}

/// Copy a tree (`/etc/skel`, the guest defaults) into the home, in the
/// child that is the guest: folders, files and links; a later tree replaces
/// what an earlier one put there. Modes are the source's permission bits
/// only (never a set-id bit), set after the content is in. What the guest
/// may not read is left out, with a line in the log.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let listing = match fs::read_dir(from) {
        Ok(listing) => listing,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            log(
                WHO,
                format_args!("{} is not readable; left out", from.display()),
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    for entry in listing {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = fs::symlink_metadata(&source)?;
        let kind = meta.file_type();
        let existing = fs::symlink_metadata(&target).ok();
        let mode = meta.mode() & 0o777;
        if kind.is_dir() {
            match &existing {
                Some(m) if m.is_dir() => {}
                Some(_) => {
                    fs::remove_file(&target)?;
                    fs::create_dir(&target)?;
                }
                None => fs::create_dir(&target)?,
            }
            // Filled first (the guest needs to write into it), then its mode.
            fs::set_permissions(&target, Permissions::from_mode(mode | 0o700))?;
            copy_tree(&source, &target)?;
            fs::set_permissions(&target, Permissions::from_mode(mode))?;
        } else if kind.is_symlink() || kind.is_file() {
            match &existing {
                Some(m) if m.is_dir() => {
                    log(
                        WHO,
                        format_args!("{} is a folder; not replaced", target.display()),
                    );
                    continue;
                }
                Some(_) => fs::remove_file(&target)?,
                None => {}
            }
            if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&source)?, &target)?;
            } else {
                let mut input = match File::open(&source) {
                    Ok(input) => input,
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        log(
                            WHO,
                            format_args!("{} is not readable; left out", source.display()),
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&target)?;
                io::copy(&mut input, &mut output)?;
                output.set_permissions(Permissions::from_mode(mode))?;
            }
        }
        // Devices, pipes, sockets: not copied.
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the guest login, in the session worker (design point 4)

/// Whether the login service registered the session PAM just opened: its
/// PAM module (`pam_systemd`, `pam_elogind`) puts `XDG_SESSION_ID` into the
/// PAM environment. The guest's PAM lines for it are optional, so this is
/// checked (design point 4).
pub fn registered(pam: &mut Pam<Talk<'_>>) -> bool {
    pam.env()
        .iter()
        .any(|pair| pair.to_bytes().starts_with(b"XDG_SESSION_ID="))
}

/// The guest lock for a session. A cleanup that holds it (or the start's
/// search for set-id files that has not finished) is waited for when
/// `wait`, at most [`WAIT_FOR_CLEANUP`], watching for a cancel. `Ok(None)`:
/// cancelled; `Err`: the line for the screen.
fn lock_for_session(channel: &Channel, wait: bool) -> Result<Option<Lock>, &'static str> {
    let deadline = Instant::now() + WAIT_FOR_CLEANUP;
    loop {
        match claim(Role::Session) {
            Ok(Claim::Got(lock)) if setid_checked() => return Ok(Some(lock)),
            Ok(Claim::Got(_)) => {
                // Nothing runs, and the start's search has not finished
                // clean: waiting would not help.
                log(
                    WHO,
                    "guest refused: the search for set-id files has not finished clean",
                );
                return Err(STILL_CLEARING);
            }
            Ok(Claim::Held(Some((Role::Session, _)))) => {
                log(WHO, "guest refused: a guest session is already running");
                return Err(ALREADY_RUNNING);
            }
            // A cleanup, or a holder being replaced.
            Ok(Claim::Held(_)) => {}
            Err(e) => {
                log(WHO, format_args!("the guest lock {LOCK_PATH}: {e}"));
                return Err(UNAVAILABLE);
            }
        }
        if !wait || Instant::now() >= deadline {
            return Err(STILL_CLEARING);
        }
        match channel.wait_readable(250) {
            Ok(true) => {
                if session::cancel_waiting(channel) {
                    return Ok(None);
                }
            }
            Ok(false) => {}
            Err(_) => return Ok(None),
        }
    }
}

/// A whole guest login, in the session worker: the lock, the cleanup, the
/// home, PAM's `reclogin-guest`, the session, and the cleanup after it.
/// The steps that must not be cut short are announced to `reclogind`
/// ([`hold`]).
pub fn worker_login(
    channel: &Channel,
    signals: &OwnedFd,
    session: &SessionRef,
    console: u32,
    wait: bool,
) -> Result<()> {
    let fail = |text: &str| channel.send(&FromWorker::Failed { text: text.into() }, &[]);

    // What to run, before anything is touched.
    let plan = match session::read(session) {
        Ok(plan) => plan,
        Err(text) => {
            fail(&text)?;
            return Ok(());
        }
    };
    let account = match account() {
        Ok(account) => account,
        Err(why) => {
            log(WHO, format_args!("guest refused: {why}"));
            fail(UNAVAILABLE)?;
            return Ok(());
        }
    };

    // One guest at a time; the cleanup must not run under a live guest.
    hold(channel, true);
    let lock = match lock_for_session(channel, wait) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            hold(channel, false);
            log(WHO, "cancelled while waiting for the guest's cleanup");
            return Ok(());
        }
        Err(text) => {
            hold(channel, false);
            fail(text)?;
            return Ok(());
        }
    };

    // A guest login only starts when the cleanup finished completely. The
    // login service is not asked here (see `ask_login_service_to_end_user`).
    let quiet = Steps {
        login_service: false,
        setid_walk: false,
    };
    let before = cleanup_locked(&lock, quiet);
    let home = match before {
        Outcome::Clean => make_home(),
        Outcome::Failed(_) => {
            report("before the guest login", &before);
            Err(String::new())
        }
    };
    hold(channel, false);

    let result = match home {
        Err(why) if why.is_empty() => fail(STILL_CLEARING).map_err(Into::into),
        Err(why) => {
            log(WHO, format_args!("the guest's home: {why}"));
            fail(NOT_STARTED).map_err(Into::into)
        }
        Ok(()) if session::cancel_waiting(channel) => {
            log(WHO, "cancelled before the guest's PAM session");
            Ok(())
        }
        Ok(()) => log_in(channel, signals, &account, &plan, console),
    };

    // The logout (design point 6): PAM is closed (`session::run`, and
    // `pam_end` when `log_in` returned); then the uid sweep, the unmounting
    // and the rest of the cleanup. `reclogind` runs the cleanup child after
    // this worker has exited, with the login service.
    hold(channel, true);
    report("after the guest logout", &cleanup_locked(&lock, quiet));
    drop(lock);
    hold(channel, false);
    result
}

/// PAM and the session, on the fresh home. Every failure before the
/// desktop's console is shown is `failed` with a plain line.
fn log_in(
    channel: &Channel,
    signals: &OwnedFd,
    account: &Account,
    plan: &session::Plan,
    console: u32,
) -> Result<()> {
    let fail = |text: &str| channel.send(&FromWorker::Failed { text: text.into() }, &[]);

    // Only the account `guest`, and no conversation: a question from any
    // module is refused (`Talk::silent`).
    let mut pam = match Pam::start(SERVICE, NAME, Talk::silent(channel)) {
        Ok(pam) => pam,
        Err(e) => {
            log(WHO, e);
            fail(NOT_STARTED)?;
            return Ok(());
        }
    };
    let rc = pam.set_item(pam::TTY, &format!("tty{console}"));
    if rc != pam::SUCCESS {
        log(WHO, format_args!("setting PAM_TTY: {}", pam.describe(rc)));
        fail(NOT_STARTED)?;
        return Ok(());
    }
    let rc = pam.authenticate();
    if pam.talker().cancelled {
        return Ok(());
    }
    if rc != pam::SUCCESS {
        log(
            WHO,
            format_args!("{SERVICE} refused the guest: {}", pam.describe(rc)),
        );
        fail(NOT_STARTED)?;
        return Ok(());
    }
    let rc = pam.acct_mgmt();
    if pam.talker().cancelled {
        return Ok(());
    }
    if rc != pam::SUCCESS {
        log(
            WHO,
            format_args!(
                "{SERVICE} refused the guest's account: {}",
                pam.describe(rc)
            ),
        );
        fail(NOT_STARTED)?;
        return Ok(());
    }
    // A module may change the name: this service is for the guest only.
    if pam.user().as_deref() != Some(NAME) {
        log(
            WHO,
            format_args!("{SERVICE} changed the user to {:?}; refused", pam.user()),
        );
        fail(NOT_STARTED)?;
        return Ok(());
    }

    channel.send(&FromWorker::Accepted, &[])?;
    session::run(&mut pam, account, plan, console, channel, signals);
    drop(pam);
    Ok(())
}
