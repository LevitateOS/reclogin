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
//! - `reclogind` at start ([`at_start`]): reads the setting, checks that the
//!   account is really the guest, and runs the cleanup before the screen is
//!   shown (also with the entry switched off, so that leftovers go).
//! - `begin {who: guest}` (`login.rs`): refused with a plain line when the
//!   setting is off or the account is not the guest; otherwise a session
//!   worker is started with `start_guest`.
//! - The session worker ([`worker_login`]): takes the guest lock, runs the
//!   cleanup, mounts and fills the home, runs PAM's `reclogin-guest` service
//!   for the account `guest` only and without a conversation, then the usual
//!   path (`session::run`: console, X or Wayland, the desktop, its end). After
//!   the session it runs the cleanup again, which ends every process of the
//!   guest and unmounts the home, and only then exits; `reclogind` shows the
//!   login screen when the worker has exited.
//! - A guest worker that went away without its cleanup, or whose cleanup did
//!   not finish: `reclogind` runs the cleanup in a thread
//!   ([`cleanup_in_background`]).
//!
//! One guest at a time: the guest lock, `/run/reclogin/guest.lock`, a `flock`
//! held by the guest's worker from before its first cleanup to the end of its
//! logout cleanup, and by a cleanup run elsewhere while it runs. The kernel
//! drops it when its holder dies, so a crashed worker never blocks the next
//! guest. Its content names the holder (`session <pid>` or `cleanup <pid>`)
//! and is read only while someone holds it. The lock is how step 1 ("if the
//! login service lists a session of user 60600, stop") is answered here: a
//! guest session reclogin started has a live worker holding it.
//!
//! "Finished completely" means the check of step 7 found nothing: no process
//! whose real or saved uid is 60600 (zombies aside), nothing mounted at or
//! under the home, `/run/media/guest` or `/run/user/60600`, none of the
//! named leftovers, nothing of uid 60600 in the shared places, no System V
//! object of uid 60600. A failed check leaves the mark
//! `/run/reclogin/guest-cleanup-failed` (removed by the next clean pass);
//! while it is there [`offered`] is false.

use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use reclogin::config::Config;
use reclogin::log;
use reclogin::wire::Channel;

use crate::pam::{self, Pam};
use crate::session::{self, Account};
use crate::worker::{FromWorker, SessionRef, Talk};

const WHO: &str = "reclogind (guest)";

/// The account (design point 1; owner's decision 6 for the number).
pub const NAME: &str = "guest";
pub const UID: libc::uid_t = 60600;
pub const GID: libc::gid_t = 60600;
pub const HOME: &str = "/run/reclogin/guest";
const HOME_C: &CStr = c"/run/reclogin/guest";

/// The PAM service of the guest (design point 4).
const SERVICE: &str = "reclogin-guest";

const RUN_DIR: &str = "/run/reclogin";
const LOCK_PATH: &str = "/run/reclogin/guest.lock";
const FAILED_MARK: &str = "/run/reclogin/guest-cleanup-failed";

/// Copied over the fresh home, in this order (design point 2, step 4).
const DEFAULTS: [&str; 2] = ["/etc/skel", "/usr/share/reclogin/guest-home"];

/// The home's size: a quarter of memory, at most 4 GiB (owner's decision 5).
const HOME_CAP: u64 = 4 << 30;
/// How long the uid sweep repeats before it gives up (design point 3).
const SWEEP_FOR: Duration = Duration::from_secs(10);
/// How deep the walk of the shared places goes; a deeper tree of the
/// guest's is not removed and fails the check.
const MAX_DEPTH: usize = 256;
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
/// login service's runtime folder.
const MOUNTED_UNDER: [&str; 3] = [HOME, "/run/media/guest", "/run/user/60600"];

/// Everything of uid 60600 in these is removed; the walk never follows a
/// symbolic link and stays on the place's own file system.
const SHARED_PLACES: [&str; 4] = ["/tmp", "/var/tmp", "/dev/shm", "/dev/mqueue"];

/// Removed whole when present (after the unmounting).
const LEFTOVERS: [&str; 9] = [
    HOME,
    "/run/media/guest",
    "/run/user/60600",
    // crontab: cronie (LevitateOS), busybox crond (AcornOS)
    "/var/spool/cron/guest",
    "/var/spool/cron/crontabs/guest",
    "/var/lib/systemd/linger/guest",
    "/var/lib/elogind/linger/guest",
    "/var/lib/AccountsService/users/guest",
    "/var/lib/AccountsService/icons/guest",
];

// ---------------------------------------------------------------------------
// the login service

/// CONNECT HERE: ask the login service to end the guest.
///
/// The design (point 3, "Ending the guest's processes", step 2, and step 2
/// of the cleanup) calls, on the system bus:
///
/// ```text
/// destination  org.freedesktop.login1
/// object       /org/freedesktop/login1
/// interface    org.freedesktop.login1.Manager
/// method       TerminateUser(u uid)        with uid = 60600
/// ```
///
/// Both systemd-logind and elogind provide it; on LevitateOS it also stops
/// the guest's `systemd --user`. Every error is ignored: this is a courtesy,
/// the uid sweep after it is the guarantee. It is not connected yet: the
/// login-service helper of "other people and power" (`logind.rs`) is to be
/// called from here. Until then nothing is asked.
fn ask_login_service_to_end_user(uid: libc::uid_t) {
    log(
        WHO,
        format_args!("TerminateUser({uid}) is not connected yet; only the uid sweep runs"),
    );
}

// ---------------------------------------------------------------------------
// the setting and the account

/// Whether `begin {who: guest}` may start a guest, as known at start.
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
    /// guest may start.
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
    let passwd = fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
    let number = UID.to_string();
    let others: Vec<&str> = passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() >= 3 && fields[2] == number && fields[0] != NAME).then_some(fields[0])
        })
        .collect();
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

/// At `reclogind`'s start, before the screen is shown: the setting, the
/// account, and the cleanup (design point 2, "Power loss"; point 6,
/// "reclogin at boot"). Leftovers are cleared with the entry switched off
/// too, but never for an account that is not the guest.
pub fn at_start(config: &Config) -> Setting {
    let account_ok = match account() {
        Ok(_) => true,
        Err(why) => {
            log(WHO, format_args!("no Guest entry: {why}"));
            false
        }
    };
    if account_ok {
        report("at start", &cleanup_now());
    }
    if !config.guest {
        log(WHO, "the Guest entry is switched off (guest = off)");
        Setting::Off
    } else if account_ok {
        Setting::On
    } else {
        Setting::Unavailable
    }
}

/// Whether the screen offers Guest now: the setting is on, the account is
/// the guest, and the last cleanup did not fail. For `state.rs`
/// (`guest_offered`), which belongs to another stage: it is to read
/// `crate::guest::offered(config)` there in place of `config.guest`. Until
/// then `main.rs` gives the screen the setting and the account as known at
/// start ([`Setting::config_for_screen`]).
#[allow(dead_code)]
pub fn offered(config: &Config) -> bool {
    config.guest && account().is_ok() && !cleanup_failed()
}

/// Whether the last cleanup found something it could not remove.
pub fn cleanup_failed() -> bool {
    fs::symlink_metadata(FAILED_MARK).is_ok()
}

/// "End the guest session" (design point 5, "Screen locking"): the running
/// guest session's worker is asked to end it (`SIGTERM`, which ends the
/// desktop, closes PAM and runs the logout cleanup). For the stage that
/// lists logged-in sessions and returns to them; false when no guest
/// session runs.
#[allow(dead_code)]
pub fn end_running_session() -> bool {
    let Ok(mut file) = File::open(LOCK_PATH) else {
        return false;
    };
    // Not held: no guest session.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let Some((Role::Session, pid)) = holder(&text) else {
        return false;
    };
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let is_worker = cmdline
        .split(|&b| b == 0)
        .any(|arg| arg == crate::worker::SWITCH.as_bytes());
    if !is_worker || unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
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

/// Held while it lives; the kernel drops it when the file is closed.
struct Lock(#[allow(dead_code)] File);

enum Claim {
    Got(Lock),
    /// Someone holds it; who, when its content says so.
    Held(Option<(Role, i32)>),
}

fn claim(role: Role) -> io::Result<Claim> {
    ensure_run_dir()?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(LOCK_PATH)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error);
        }
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        return Ok(Claim::Held(holder(&text)));
    }
    let word = match role {
        Role::Session => "session",
        Role::Cleanup => "cleanup",
    };
    file.set_len(0)?;
    writeln!(file, "{word} {}", std::process::id())?;
    Ok(Claim::Got(Lock(file)))
}

fn holder(text: &str) -> Option<(Role, i32)> {
    let mut words = text.split_whitespace();
    let role = match words.next()? {
        "session" => Role::Session,
        "cleanup" => Role::Cleanup,
        _ => return None,
    };
    Some((role, words.next()?.parse().ok()?))
}

/// `/run/reclogin`, root's, mode 0755 (design point 2, step 1).
fn ensure_run_dir() -> io::Result<()> {
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
    /// Not run: a guest session runs, or another cleanup does, or there is
    /// no guest account.
    Skipped(String),
    /// Run, and the check found something left.
    Failed(String),
}

fn report(when: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Clean => log(WHO, format_args!("cleanup {when}: clean")),
        Outcome::Skipped(why) => log(WHO, format_args!("cleanup {when}: not run, {why}")),
        Outcome::Failed(why) => log(
            WHO,
            format_args!("CLEANUP {when} DID NOT FINISH; no guest can start until it does: {why}"),
        ),
    }
}

/// The cleanup, with the guest lock, for `reclogind` (at start, after a
/// guest worker that went away). Skipped while a guest session runs.
pub fn cleanup_now() -> Outcome {
    if let Err(why) = account() {
        return Outcome::Skipped(why);
    }
    match claim(Role::Cleanup) {
        Ok(Claim::Got(lock)) => cleanup_locked(&lock),
        Ok(Claim::Held(Some((Role::Cleanup, _)))) => {
            Outcome::Skipped("another cleanup is running".into())
        }
        Ok(Claim::Held(_)) => Outcome::Skipped("a guest session is running".into()),
        Err(e) => Outcome::Failed(format!("the guest lock {LOCK_PATH}: {e}")),
    }
}

/// The cleanup in a thread of `reclogind`, which keeps serving the screen.
pub fn cleanup_in_background() {
    let spawned = std::thread::Builder::new()
        .name("guest-cleanup".into())
        .spawn(|| report("after the guest's worker", &cleanup_now()));
    if let Err(e) = spawned {
        log(WHO, format_args!("starting the cleanup: {e}"));
    }
}

/// Steps 2 to 7 of the routine; the caller holds the guest lock (step 1).
/// Each step is safe to repeat. Sets or clears the failure mark.
fn cleanup_locked(_held: &Lock) -> Outcome {
    // 2. The login service, then the uid sweep.
    ask_login_service_to_end_user(UID);
    if !sweep() {
        log(WHO, "processes of the guest are left after the sweep");
    }
    // 3. Everything mounted under the guest's places, then the folders.
    unmount_all();
    // 3 and 5. The folders and the named leftovers.
    for path in LEFTOVERS {
        if let Err(e) = remove_path(path) {
            log(WHO, format_args!("removing {path}: {e}"));
        }
    }
    // 4. The guest's files in the shared writable places.
    for place in SHARED_PLACES {
        let walk = shared_place(place, true);
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
    // 6. System V shared memory, semaphores, message queues.
    remove_ipc();
    // 7. The check.
    let left = check();
    let failed = !left.is_empty();
    set_failed_mark(failed);
    if failed {
        Outcome::Failed(left.join("; "))
    } else {
        Outcome::Clean
    }
}

fn set_failed_mark(failed: bool) {
    if failed {
        if let Err(e) = ensure_run_dir().and_then(|()| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(FAILED_MARK)
                .map(drop)
        }) {
            log(WHO, format_args!("writing {FAILED_MARK}: {e}"));
        }
    } else if let Err(e) = fs::remove_file(FAILED_MARK) {
        if e.kind() != io::ErrorKind::NotFound {
            log(WHO, format_args!("removing {FAILED_MARK}: {e}"));
        }
    }
}

/// Step 7: what is left. Empty means finished completely.
fn check() -> Vec<String> {
    let mut left = Vec::new();
    let processes = guest_processes();
    if processes > 0 {
        left.push(format!("{processes} processes of uid {UID}"));
    }
    match mounts_under() {
        Ok(mounts) if mounts.is_empty() => {}
        Ok(mounts) => left.push(format!("still mounted: {}", mounts.join(", "))),
        Err(e) => left.push(format!("the mounts cannot be read: {e}")),
    }
    for path in LEFTOVERS {
        if fs::symlink_metadata(path).is_ok() {
            left.push(format!("still there: {path}"));
        }
    }
    for place in SHARED_PLACES {
        let walk = shared_place(place, false);
        if walk.found > 0 {
            left.push(format!("{} of the guest's files under {place}", walk.found));
        }
    }
    let ipc = ipc_objects(true).len();
    if ipc > 0 {
        left.push(format!("{ipc} System V objects of uid {UID}"));
    }
    left
}

// ---------------------------------------------------------------------------
// processes (design point 3, "Ending the guest's processes")

/// How many processes have the real or saved uid 60600 (the ones the sweep
/// can signal), zombies not counted. An unreadable `/proc` counts as one, so
/// that the check fails rather than passes blind.
fn guest_processes() -> usize {
    let Ok(dir) = fs::read_dir("/proc") else {
        return 1;
    };
    dir.flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) && is_guests(&name)
        })
        .count()
}

fn is_guests(pid: &str) -> bool {
    let Ok(text) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let (mut ours, mut dead) = (false, false);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            dead = matches!(rest.trim_start().chars().next(), Some('Z' | 'X'));
        } else if let Some(rest) = line.strip_prefix("Uid:") {
            let ids: Vec<u32> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            ours = ids.first() == Some(&UID) || ids.get(2) == Some(&UID);
        }
    }
    ours && !dead
}

/// The uid sweep: a helper drops to uid 60600 and sends `SIGKILL` to every
/// process it may signal, which is every process of the guest, wherever it
/// hides; repeated until `/proc` shows none, for up to ten seconds.
fn sweep() -> bool {
    let deadline = Instant::now() + SWEEP_FOR;
    loop {
        if guest_processes() == 0 {
            return true;
        }
        if !kill_as_guest() {
            return false;
        }
        if Instant::now() >= deadline {
            return guest_processes() == 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill_as_guest() -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        log(
            WHO,
            format_args!("starting the sweep: {}", io::Error::last_os_error()),
        );
        return false;
    }
    if pid == 0 {
        // The helper: only system calls until `_exit`. It never signals
        // anything unless it really is uid 60600 and nothing else.
        unsafe {
            if libc::syscall(libc::SYS_setgroups, 0usize, std::ptr::null::<libc::gid_t>()) != 0
                || libc::syscall(libc::SYS_setresgid, GID, GID, GID) != 0
                || libc::syscall(libc::SYS_setresuid, UID, UID, UID) != 0
                || libc::getuid() != UID
                || libc::geteuid() != UID
                || libc::getgid() != GID
            {
                libc::_exit(1);
            }
            libc::kill(-1, libc::SIGKILL);
            libc::_exit(0);
        }
    }
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
    let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    if !ok {
        log(WHO, "the sweep helper could not become the guest");
    }
    ok
}

// ---------------------------------------------------------------------------
// mounts

/// Mount points at or under [`MOUNTED_UNDER`], from `/proc/self/mountinfo`.
fn mounts_under() -> io::Result<Vec<String>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(text
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape)
        .filter(|point| {
            MOUNTED_UNDER.iter().any(|root| {
                point == root
                    || point
                        .strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .collect())
}

/// mountinfo writes space, tab, newline and backslash as `\ooo`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
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
    String::from_utf8_lossy(&out).into_owned()
}

/// Unmount everything under the guest's places, deepest first. A busy
/// mount is detached (lazy unmount): unreachable by path at once, freed when
/// the last stuck reference goes (design point 2, "A wipe that did not
/// finish").
fn unmount_all() {
    for _ in 0..8 {
        let mut mounts = match mounts_under() {
            Ok(mounts) => mounts,
            Err(e) => {
                log(WHO, format_args!("reading the mounts: {e}"));
                return;
            }
        };
        if mounts.is_empty() {
            return;
        }
        mounts.sort_by_key(|m| std::cmp::Reverse(m.len()));
        for point in mounts {
            let Ok(c_point) = CString::new(point.as_str()) else {
                continue;
            };
            if unsafe { libc::umount2(c_point.as_ptr(), libc::UMOUNT_NOFOLLOW) } == 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EBUSY) => {
                    let rc = unsafe {
                        libc::umount2(c_point.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW)
                    };
                    if rc == 0 {
                        log(WHO, format_args!("{point} was busy; detached"));
                    } else {
                        log(
                            WHO,
                            format_args!("detaching {point}: {}", io::Error::last_os_error()),
                        );
                    }
                }
                // Gone meanwhile.
                Some(libc::EINVAL) | Some(libc::ENOENT) => {}
                _ => log(WHO, format_args!("unmounting {point}: {error}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// files: every walk works from open folder handles (`openat`, `fstatat`,
// `unlinkat`), never follows a symbolic link and never leaves the file
// system it started on, so nobody can steer it elsewhere by swapping a
// folder for a link while it runs.

fn open_dir(path: &str) -> io::Result<OwnedFd> {
    let c_path = CString::new(path).map_err(|_| io::ErrorKind::InvalidInput)?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

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

#[derive(Default)]
struct Walk {
    /// The file system the walk stays on.
    dev: libc::dev_t,
    /// Things of the guest's seen (with `delete`, also the ones removed).
    found: usize,
    /// Things of the guest's that could not be removed.
    failed: usize,
}

impl Walk {
    /// `all`: everything here is the guest's (inside a folder of its own).
    fn walk(&mut self, dir: RawFd, depth: usize, all: bool, delete: bool) {
        let names = match entries(dir) {
            Ok(names) => names,
            Err(_) => {
                if all {
                    self.failed += 1;
                }
                return;
            }
        };
        for name in names {
            let Ok(st) = stat_at(dir, &name) else {
                continue;
            };
            let ours = all || st.st_uid == UID;
            if st.st_dev != self.dev {
                // Another file system mounted here: not walked.
                if ours {
                    self.found += 1;
                    self.failed += 1;
                }
                continue;
            }
            if !is_dir(&st) {
                if ours {
                    self.found += 1;
                    if delete && unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
                        self.failed += 1;
                    }
                }
                continue;
            }
            if ours && !delete {
                self.found += 1;
                continue;
            }
            if depth >= MAX_DEPTH {
                if ours {
                    self.found += 1;
                    self.failed += 1;
                }
                continue;
            }
            match open_dir_at(dir, &name) {
                Ok(child) => self.walk(child.as_raw_fd(), depth + 1, ours, delete),
                Err(_) => {
                    if ours {
                        self.failed += 1;
                    }
                }
            }
            if ours {
                self.found += 1;
                if unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                    self.failed += 1;
                }
            }
        }
    }
}

/// Step 4 for one shared place: the guest's files found (and with `delete`
/// removed). A place that does not exist (or is a link) has none.
fn shared_place(place: &str, delete: bool) -> Walk {
    let Ok(dir) = open_dir(place) else {
        return Walk::default();
    };
    let Ok(st) = fstat(dir.as_raw_fd()) else {
        return Walk::default();
    };
    let mut walk = Walk {
        dev: st.st_dev,
        ..Walk::default()
    };
    walk.walk(dir.as_raw_fd(), 0, false, delete);
    walk
}

/// Remove `path` whole (a folder with everything in it), without following
/// a link at its end or inside. Missing is fine.
fn remove_path(path: &str) -> io::Result<()> {
    let path = Path::new(path);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let parent = match open_dir(&parent.to_string_lossy()) {
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
        let mut walk = Walk {
            dev: st.st_dev,
            ..Walk::default()
        };
        walk.walk(dir.as_raw_fd(), 0, true, true);
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
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
/// defaults copied in. The error is for the log; the cleanup after the
/// login undoes whatever was done.
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

    for source in DEFAULTS {
        if Path::new(source).is_dir() {
            copy_tree(Path::new(source), Path::new(HOME))
                .map_err(|e| format!("copying {source}: {e}"))?;
        }
    }
    Ok(())
}

/// Copy a tree of root's (`/etc/skel`, the guest defaults) into the home,
/// owned by the guest: folders, files and links; a later tree replaces what
/// an earlier one put there. Set-id bits are not copied. Nothing of the
/// guest's runs yet, the home is fresh.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = fs::symlink_metadata(&source)?;
        let kind = meta.file_type();
        let existing = fs::symlink_metadata(&target).ok();
        let mode = Permissions::from_mode(meta.mode() & 0o777);
        if kind.is_dir() {
            match &existing {
                Some(m) if m.is_dir() => {}
                Some(_) => {
                    fs::remove_file(&target)?;
                    fs::create_dir(&target)?;
                }
                None => fs::create_dir(&target)?,
            }
            std::os::unix::fs::lchown(&target, Some(UID), Some(GID))?;
            fs::set_permissions(&target, mode)?;
            copy_tree(&source, &target)?;
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
                std::os::unix::fs::lchown(&target, Some(UID), Some(GID))?;
            } else {
                fs::copy(&source, &target)?;
                std::os::unix::fs::lchown(&target, Some(UID), Some(GID))?;
                fs::set_permissions(&target, mode)?;
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

/// A whole guest login, in the session worker: the lock, the cleanup, the
/// home, PAM's `reclogin-guest`, the session, and the cleanup after it.
pub fn worker_login(
    channel: &Channel,
    signals: &OwnedFd,
    session: &SessionRef,
    console: u32,
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
    let lock = match claim(Role::Session) {
        Ok(Claim::Got(lock)) => lock,
        Ok(Claim::Held(Some((Role::Cleanup, _)))) => {
            fail(STILL_CLEARING)?;
            return Ok(());
        }
        Ok(Claim::Held(_)) => {
            log(WHO, "guest refused: a guest session is already running");
            fail(ALREADY_RUNNING)?;
            return Ok(());
        }
        Err(e) => {
            log(WHO, format_args!("the guest lock {LOCK_PATH}: {e}"));
            fail(UNAVAILABLE)?;
            return Ok(());
        }
    };

    // A guest login only starts when the cleanup finished completely.
    let before = cleanup_locked(&lock);
    if !matches!(before, Outcome::Clean) {
        report("before the guest login", &before);
        fail(STILL_CLEARING)?;
        return Ok(());
    }

    let result = log_in(channel, signals, &account, &plan, console);

    // The logout (design point 6): PAM is closed (`session::run`, and
    // `pam_end` when `log_in` returned); then the login service, the uid
    // sweep, the unmounting and the rest of the cleanup.
    report("after the guest logout", &cleanup_locked(&lock));
    drop(lock);
    result
}

/// From the home to the end of the session. Every failure before the
/// desktop's console is shown is `failed` with a plain line.
fn log_in(
    channel: &Channel,
    signals: &OwnedFd,
    account: &Account,
    plan: &session::Plan,
    console: u32,
) -> Result<()> {
    let fail = |text: &str| channel.send(&FromWorker::Failed { text: text.into() }, &[]);

    if let Err(why) = make_home() {
        log(WHO, format_args!("the guest's home: {why}"));
        fail(NOT_STARTED)?;
        return Ok(());
    }

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
