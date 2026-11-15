//! Starting and ending a desktop session, in the session worker, after PAM
//! accepted the person.
//!
//! The order (see `README.md`, "What a login does"): the console is handed
//! to the user, the PAM session is opened with the console and seat in the
//! PAM environment (the login service's PAM module reads them), `reclogind`
//! shows that console, and then the desktop's process is forked: a session
//! of its own with the console as its controlling terminal and standard
//! input, the groups the worker has after `initgroups` and PAM's credential
//! modules (as login(1)), the user's ids, PAM's environment, its output and
//! errors in the user's own `session.log`, and the session file's `Exec`
//! (split by the Desktop Entry rules, each word quoted for the shell) run
//! through the user's login shell so their profile is read. When it exits
//! the PAM session is closed and the console given back to root.
//!
//! An X session differs in two places: the X server is started for the
//! user on the console ([`crate::xserver`]) after the PAM session is open and
//! before `reclogind` is told the session is ready, and the desktop's
//! process gets `DISPLAY` and `XAUTHORITY` but not the console as its
//! controlling terminal (the X server holds it). The server is stopped when
//! the desktop's process has ended, before the PAM session is closed.

use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::SessionKind;
use reclogin::wire::Channel;

use crate::pam::{self, Pam};
use crate::sessions::{WAYLAND_DIR, X11_DIR};
use crate::worker::{hold, next_signal, receive, Command, FromWorker, SessionRef, Talk};
use crate::xserver;

const WHO: &str = "reclogind (session worker)";

const NOT_STARTED: &str = "The session could not be started.";

/// The `PATH` a session gets when PAM's environment names none.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

const VT_GETSTATE: libc::c_ulong = 0x5603;
const TIOCSCTTY: libc::c_ulong = 0x540E;
const TIOCVHANGUP: libc::c_ulong = 0x5437;

/// How long the worker waits for `reclogind` to show the session's console
/// before starting the desktop anyway.
const SHOW_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a desktop has to end after `SIGTERM` before `SIGKILL`.
const END_TIMEOUT: Duration = Duration::from_secs(10);

/// What a session file says to run.
pub struct Plan {
    pub id: String,
    pub kind: SessionKind,
    pub exec: String,
    /// `DesktopNames=`, for `XDG_CURRENT_DESKTOP`.
    pub desktop_names: Vec<String>,
}

/// Read the session file `reclogind` chose. The error is the line for the
/// screen.
pub fn read(session: &SessionRef) -> Result<Plan, String> {
    let id = &session.id;
    if id.is_empty() || id.starts_with('.') || id.contains('/') {
        return Err(NOT_STARTED.into());
    }
    let dir = match session.kind {
        SessionKind::Wayland => WAYLAND_DIR,
        // Before anyone types a password for a session that cannot start.
        SessionKind::X11 => {
            if xserver::program().is_none() {
                log(WHO, "an X session was chosen, but no X server is installed");
                return Err(xserver::NO_SERVER.into());
            }
            X11_DIR
        }
    };
    let path = Path::new(dir).join(format!("{id}.desktop"));
    let text = std::fs::read_to_string(&path).map_err(|e| {
        log(WHO, format_args!("reading {}: {e}", path.display()));
        NOT_STARTED.to_string()
    })?;
    let (mut exec, mut names, mut in_entry) = (None, Vec::new(), false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => exec = Some(value.trim().to_string()),
            "DesktopNames" => {
                names = value
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            _ => {}
        }
    }
    let exec = exec.filter(|e| !e.is_empty()).ok_or_else(|| {
        log(WHO, format_args!("{} has no Exec", path.display()));
        NOT_STARTED.to_string()
    })?;
    Ok(Plan {
        id: id.clone(),
        kind: session.kind,
        exec,
        desktop_names: names,
    })
}

/// The account a session runs as, read before anything is forked.
pub struct Account {
    pub name: String,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub home: String,
    pub shell: String,
}

impl Account {
    pub fn lookup(name: &str) -> io::Result<Account> {
        let c_name = CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such account"));
        }
        let text = |p: *const libc::c_char| {
            if p.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };
        let (uid, gid) = (pwd.pw_uid, pwd.pw_gid);
        let home = text(pwd.pw_dir);
        let mut shell = text(pwd.pw_shell);
        if shell.is_empty() {
            shell = "/bin/sh".into();
        }
        Ok(Account {
            name: text(pwd.pw_name),
            uid,
            gid,
            home,
            shell,
        })
    }
}

fn group_id(name: &str) -> Option<libc::gid_t> {
    let c_name = CString::new(name).ok()?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &mut grp,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    (rc == 0 && !result.is_null()).then_some(grp.gr_gid)
}

/// The console device, handed to the user and given back at the end.
struct Tty {
    fd: OwnedFd,
}

impl Tty {
    fn hand_over(number: u32, account: &Account) -> io::Result<Tty> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/dev/tty{number}"))?;
        let tty = Tty { fd: file.into() };
        let group = group_id("tty").unwrap_or(account.gid);
        if unsafe { libc::fchown(tty.fd.as_raw_fd(), account.uid, group) } != 0
            || unsafe { libc::fchmod(tty.fd.as_raw_fd(), 0o620) } != 0
        {
            let error = io::Error::last_os_error();
            tty.give_back();
            return Err(error);
        }
        Ok(tty)
    }

    /// Cut off whatever of the session still holds the console, and give it
    /// back to root.
    fn give_back(&self) {
        give_console_back(self.fd.as_raw_fd());
    }
}

/// A console device as nobody's: whatever still has it open is cut off
/// (hang-up), and it is root's, group `tty` (root's group when there is
/// none), mode 0620. Never a remembered owner: that could be a user whose
/// session was not closed. Used by the worker at the end of a session and by
/// `reclogind` after a worker that did not close its session.
pub fn give_console_back(fd: RawFd) {
    let group = group_id("tty").unwrap_or(0);
    unsafe {
        libc::ioctl(fd, TIOCVHANGUP as _, 0 as libc::c_long);
        if libc::fchown(fd, 0, group) != 0 || libc::fchmod(fd, 0o620) != 0 {
            log(
                WHO,
                format_args!("giving the console back: {}", io::Error::last_os_error()),
            );
        }
    }
}

/// Open the PAM session on `console`, start the desktop, wait for its end
/// and close everything. Reports `ReadyToStart`, `Started` and `Ended`, or
/// `Failed` when it ends before the console was shown.
pub fn run(
    pam: &mut Pam<Talk<'_>>,
    account: &Account,
    plan: &Plan,
    console: u32,
    channel: &Channel,
    signals: &OwnedFd,
) {
    let send = |message: &FromWorker| {
        if let Err(e) = channel.send(message, &[]) {
            log(WHO, format_args!("sending: {e}"));
        }
    };
    let failed = || {
        send(&FromWorker::Failed {
            text: NOT_STARTED.into(),
        })
    };

    // A cancel that came while PAM checked the account: nothing is handed
    // over or opened.
    if cancel_waiting(channel) {
        log(WHO, "cancelled before the session was opened");
        return;
    }
    let tty = match Tty::hand_over(console, account) {
        Ok(tty) => tty,
        Err(e) => {
            log(
                WHO,
                format_args!("handing console {console} to {}: {e}", account.name),
            );
            failed();
            return;
        }
    };

    // What the login service's PAM module reads.
    let desktop = plan
        .desktop_names
        .first()
        .cloned()
        .unwrap_or_else(|| plan.id.clone());
    let vtnr = console.to_string();
    let session_type = match plan.kind {
        SessionKind::Wayland => "wayland",
        SessionKind::X11 => "x11",
    };
    // PAM_TTY was set before authentication (`worker.rs`).
    let mut rc = pam::SUCCESS;
    for (name, value) in [
        ("XDG_SEAT", "seat0"),
        ("XDG_VTNR", vtnr.as_str()),
        ("XDG_SESSION_TYPE", session_type),
        ("XDG_SESSION_CLASS", "user"),
        ("XDG_SESSION_DESKTOP", desktop.as_str()),
    ] {
        if rc == pam::SUCCESS {
            rc = pam.putenv(name, value);
        }
    }
    if rc != pam::SUCCESS {
        log(WHO, format_args!("PAM items: {}", pam.describe(rc)));
        tty.give_back();
        failed();
        return;
    }

    // The account's groups first, so that PAM's credential modules
    // (pam_group) add to them; the session keeps the result (as login(1)).
    let Ok(c_name) = CString::new(account.name.as_str()) else {
        tty.give_back();
        failed();
        return;
    };
    if unsafe { libc::initgroups(c_name.as_ptr(), account.gid) } != 0 {
        log(
            WHO,
            format_args!(
                "reading {}'s groups: {}",
                account.name,
                io::Error::last_os_error()
            ),
        );
        tty.give_back();
        failed();
        return;
    }
    let rc = pam.setcred(pam::ESTABLISH_CRED);
    if rc != pam::SUCCESS {
        log(WHO, format_args!("credentials: {}", pam.describe(rc)));
        tty.give_back();
        failed();
        return;
    }
    // Opening and closing the PAM session must not be cut short
    // (`reclogind` holds its kill timer meanwhile, `login.rs`).
    hold(channel, true);
    let rc = pam.open_session();
    hold(channel, false);
    if rc != pam::SUCCESS {
        log(
            WHO,
            format_args!("opening the session: {}", pam.describe(rc)),
        );
        pam.setcred(pam::DELETE_CRED);
        tty.give_back();
        failed();
        return;
    }
    // As util-linux's login does; a failure here is not fatal.
    let rc = pam.setcred(pam::REINITIALIZE_CRED);
    if rc != pam::SUCCESS {
        log(
            WHO,
            format_args!("refreshing credentials: {}", pam.describe(rc)),
        );
    }

    let close = |pam: &mut Pam<Talk<'_>>| {
        hold(channel, true);
        let rc = pam.close_session();
        hold(channel, false);
        if rc != pam::SUCCESS {
            log(
                WHO,
                format_args!("closing the session: {}", pam.describe(rc)),
            );
        }
        pam.setcred(pam::DELETE_CRED);
        tty.give_back();
    };

    // The guest's PAM lines for the login service are optional (one file
    // for both systems): its session must be seen registered (`guest.rs`).
    if account.uid == crate::guest::UID && !crate::guest::registered(pam) {
        log(
            WHO,
            "the login service did not register the guest's session; not started",
        );
        close(pam);
        failed();
        return;
    }

    let mut prepared = match Prepared::new(pam, account, plan) {
        Ok(prepared) => prepared,
        Err(e) => {
            log(WHO, format_args!("preparing the session: {e}"));
            close(pam);
            failed();
            return;
        }
    };

    // An X session: the X server first, while the greeter can still be told
    // that it did not start.
    let mut x = None;
    if plan.kind == SessionKind::X11 {
        match xserver::start(
            account,
            console,
            tty.fd.as_raw_fd(),
            &prepared.env,
            channel,
            signals,
        ) {
            Ok(server) => {
                let display = server.display().to_string();
                let auth = server.auth_path().to_string_lossy().into_owned();
                for (name, value) in [("DISPLAY", display), ("XAUTHORITY", auth)] {
                    let prefix = format!("{name}=");
                    prepared
                        .env
                        .retain(|e| !e.to_bytes().starts_with(prefix.as_bytes()));
                    match CString::new(format!("{name}={value}")) {
                        Ok(pair) => prepared.env.push(pair),
                        Err(_) => log(WHO, format_args!("{name} holds a zero byte")),
                    }
                }
                x = Some(server);
            }
            Err(xserver::StartError::Failed(text)) => {
                close(pam);
                send(&FromWorker::Failed { text: text.into() });
                return;
            }
            Err(xserver::StartError::Cancelled) => {
                close(pam);
                send(&FromWorker::Ended { status: -1 });
                return;
            }
        }
    }

    // From here the greeter is gone: every end is `Ended`. The X server's
    // process descriptor goes with it: `reclogind` signals the server's
    // process group only while that process lives.
    let x_pidfd = x.as_ref().and_then(|x| x.pidfd());
    let ready = FromWorker::ReadyToStart {
        console,
        x_server: x.as_ref().map(|x| x.pid()),
        x_pidfd: x_pidfd.is_some(),
    };
    let handles: Vec<_> = x_pidfd.iter().map(|fd| fd.as_fd()).collect();
    if let Err(e) = channel.send(&ready, &handles) {
        log(WHO, format_args!("sending: {e}"));
    }
    if !wait_until_shown(console, channel) {
        log(WHO, "cancelled before the session started");
        if let Some(x) = x.as_mut() {
            x.stop();
        }
        close(pam);
        send(&FromWorker::Ended { status: -1 });
        return;
    }

    let pid = match prepared.spawn(&tty) {
        Ok(pid) => pid,
        Err(e) => {
            log(WHO, format_args!("starting {}: {e}", plan.exec));
            if let Some(x) = x.as_mut() {
                x.stop();
            }
            close(pam);
            send(&FromWorker::Ended { status: -1 });
            return;
        }
    };
    log(
        WHO,
        format_args!(
            "{} logged in on console {console}: {}",
            account.name, plan.id
        ),
    );
    // The desktop runs: from here the worker outlives `reclogind` (no
    // signal when it dies, its socket's loss ignored) and closes the
    // session by itself when the desktop ends.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, 0 as libc::c_ulong, 0, 0, 0);
    }
    // Our child, not reaped: the number is still the desktop's process.
    let desktop_pidfd = pidfd_open(pid);
    let started = FromWorker::Started {
        console,
        desktop: pid,
        pidfd: desktop_pidfd.is_some(),
    };
    let handles: Vec<_> = desktop_pidfd.iter().map(|fd| fd.as_fd()).collect();
    if let Err(e) = channel.send(&started, &handles) {
        log(WHO, format_args!("sending: {e}"));
    }
    drop(desktop_pidfd);

    let status = match x.as_mut() {
        Some(x) => x.wait_for_end(pid, channel, signals),
        None => wait_for_end(pid, channel, signals),
    };
    log(WHO, format_args!("{}'s session ended", account.name));
    if let Some(x) = x.as_mut() {
        x.stop();
    }
    close(pam);
    send(&FromWorker::Ended { status });
}

/// Everything the forked process needs, made before the fork.
struct Prepared {
    shell: CString,
    argv: Vec<CString>,
    env: Vec<CString>,
    home: CString,
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// Folders to make (as the user, in order) and the file the desktop's
    /// output and errors go to: `$XDG_STATE_HOME/reclogin/session.log`, else
    /// `~/.local/state/reclogin/session.log`.
    log_dirs: Vec<CString>,
    log_file: CString,
    /// Whether the console becomes the process's controlling terminal: not
    /// in an X session, whose X server has it already.
    controlling_tty: bool,
    /// The guest's session makes no set-id file (`nosetid.rs`): installed
    /// in the child just before `exec`.
    filter: Option<crate::nosetid::Filter>,
}

impl Prepared {
    fn new(pam: &mut Pam<Talk<'_>>, account: &Account, plan: &Plan) -> io::Result<Prepared> {
        let bad = |_| io::Error::from(io::ErrorKind::InvalidInput);
        let mut env: Vec<(String, String)> = Vec::new();
        for pair in pam.env() {
            if let Some((name, value)) = pair.to_string_lossy().split_once('=') {
                env.push((name.to_string(), value.to_string()));
            }
        }
        let has = |env: &[(String, String)], name: &str| env.iter().any(|(n, _)| n == name);
        let set = |env: &mut Vec<(String, String)>, name: &str, value: &str| {
            env.retain(|(n, _)| n != name);
            env.push((name.to_string(), value.to_string()));
        };
        set(&mut env, "HOME", &account.home);
        set(&mut env, "USER", &account.name);
        set(&mut env, "LOGNAME", &account.name);
        set(&mut env, "SHELL", &account.shell);
        if !has(&env, "PATH") {
            set(&mut env, "PATH", DEFAULT_PATH);
        }
        if !plan.desktop_names.is_empty() {
            set(
                &mut env,
                "XDG_CURRENT_DESKTOP",
                &plan.desktop_names.join(":"),
            );
        }

        // A session bus: the login service's, or one for this session.
        let bus = has(&env, "DBUS_SESSION_BUS_ADDRESS") || user_bus_appears(account.uid);
        let words = exec_words(&plan.exec).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the session's Exec cannot be read: {:?}", plan.exec),
            )
        })?;
        let quoted: Vec<String> = words.iter().map(|w| shell_quote(w)).collect();
        let command = if bus {
            format!("exec {}", quoted.join(" "))
        } else {
            format!("exec dbus-run-session -- {}", quoted.join(" "))
        };
        let shell = login_shell(&account.shell);

        // The session's own log, in the user's state folder.
        let state_home = env
            .iter()
            .find(|(n, v)| n == "XDG_STATE_HOME" && v.starts_with('/'))
            .map(|(_, v)| v.clone());
        let mut dirs = Vec::new();
        let state = match state_home {
            Some(dir) => {
                dirs.push(dir.clone());
                dir
            }
            None => {
                let local = format!("{}/.local", account.home.trim_end_matches('/'));
                dirs.push(local.clone());
                let state = format!("{local}/state");
                dirs.push(state.clone());
                state
            }
        };
        let own = format!("{}/reclogin", state.trim_end_matches('/'));
        dirs.push(own.clone());
        let log_file = CString::new(format!("{own}/session.log")).map_err(bad)?;
        let log_dirs = dirs
            .into_iter()
            .map(CString::new)
            .collect::<Result<_, _>>()
            .map_err(bad)?;

        Ok(Prepared {
            shell: CString::new(shell).map_err(bad)?,
            argv: vec![
                CString::new(shell).map_err(bad)?,
                CString::new("-l").map_err(bad)?,
                CString::new("-c").map_err(bad)?,
                CString::new(command).map_err(bad)?,
            ],
            env: env
                .into_iter()
                .map(|(n, v)| CString::new(format!("{n}={v}")))
                .collect::<Result<_, _>>()
                .map_err(bad)?,
            home: CString::new(account.home.as_str()).map_err(bad)?,
            uid: account.uid,
            gid: account.gid,
            log_dirs,
            log_file,
            controlling_tty: plan.kind != SessionKind::X11,
            filter: (account.uid == crate::guest::UID).then(crate::nosetid::Filter::new),
        })
    }

    /// Fork the desktop's process. The worker is single-threaded, so the
    /// child may call anything; it still does only system calls.
    fn spawn(&self, tty: &Tty) -> io::Result<libc::pid_t> {
        let mut argv: Vec<*const libc::c_char> = self.argv.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> = self.env.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        let tty_fd = tty.fd.as_raw_fd();

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid > 0 {
            return Ok(pid);
        }
        // The child.
        unsafe {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            // Rust ignores SIGPIPE in its programs; a desktop must not inherit that.
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            if libc::setsid() < 0
                || (self.controlling_tty
                    && libc::ioctl(tty_fd, TIOCSCTTY as _, 0 as libc::c_long) < 0)
                || libc::dup2(tty_fd, 0) < 0
            {
                libc::_exit(126);
            }
            libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
            // The groups stay the worker's: the account's (initgroups) and
            // what PAM's credential modules added.
            if libc::setgid(self.gid) != 0
                || libc::setuid(self.uid) != 0
                || (self.uid != 0 && libc::setuid(0) == 0)
            {
                libc::_exit(126);
            }
            // Output and errors into the user's own file, opened as the
            // user, not into reclogind's log, where any line of the user's
            // would carry reclogind's name. /dev/null when that fails.
            for dir in &self.log_dirs {
                libc::mkdir(dir.as_ptr(), 0o700);
            }
            let mut out = libc::open(
                self.log_file.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_TRUNC
                    | libc::O_NOFOLLOW
                    | libc::O_NOCTTY
                    | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            );
            if out >= 0 {
                libc::fchmod(out, 0o600);
            } else {
                out = libc::open(
                    c"/dev/null".as_ptr(),
                    libc::O_WRONLY | libc::O_NOCTTY | libc::O_CLOEXEC,
                );
            }
            if out >= 0 {
                libc::dup2(out, 1);
                libc::dup2(out, 2);
                if out > 2 {
                    libc::close(out);
                }
            }
            if libc::chdir(self.home.as_ptr()) != 0 {
                libc::chdir(c"/".as_ptr());
            }
            if let Some(filter) = &self.filter {
                if filter.install().is_err() {
                    libc::_exit(126);
                }
            }
            libc::execve(self.shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
}

/// Whether the login service gives this user a session bus. On a system
/// started by systemd the user's bus can appear a moment after the PAM
/// session opened, so it is waited for there; elsewhere (OpenRC with
/// elogind) it is looked for once.
fn user_bus_appears(uid: libc::uid_t) -> bool {
    let bus = format!("/run/user/{uid}/bus");
    let deadline = Instant::now()
        + if Path::new("/run/systemd/system").is_dir() {
            Duration::from_secs(3)
        } else {
            Duration::ZERO
        };
    loop {
        if Path::new(&bus).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

/// Wait until `console` is shown (or a little while), watching for a
/// cancel. False when cancelled.
fn wait_until_shown(console: u32, channel: &Channel) -> bool {
    let tty0 = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open("/dev/tty0");
    let deadline = Instant::now() + SHOW_TIMEOUT;
    loop {
        if let Ok(tty0) = &tty0 {
            let mut state = VtStat::default();
            let rc = unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, &mut state) };
            if rc == 0 && u32::from(state.v_active) == console {
                return true;
            }
        }
        if Instant::now() >= deadline {
            log(
                WHO,
                format_args!("console {console} was not shown; starting anyway"),
            );
            return true;
        }
        match channel.wait_readable(50) {
            Ok(true) => match receive(channel) {
                Ok(Some(Command::Cancel)) | Ok(None) | Err(_) => return false,
                Ok(Some(_)) => {}
            },
            Ok(false) => {}
            Err(_) => return false,
        }
    }
}

/// Wait until the desktop's process exits; end it when asked to. A lost
/// `reclogind` is not a reason to end it (see `worker.rs`).
fn wait_for_end(pid: libc::pid_t, channel: &Channel, signals: &OwnedFd) -> i32 {
    let mut channel_open = true;
    loop {
        if let Some(status) = reap(pid) {
            return status;
        }
        let mut fds = [
            libc::pollfd {
                fd: signals.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if channel_open {
                    channel.as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return end(pid);
        }
        let mut stop = false;
        if fds[0].revents != 0 {
            while let Some(signal) = next_signal(signals) {
                if signal != libc::SIGCHLD {
                    log(WHO, format_args!("ending the session on signal {signal}"));
                    stop = true;
                }
            }
        }
        if fds[1].revents != 0 {
            match receive(channel) {
                Ok(Some(Command::Cancel)) => stop = true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    log(WHO, "reclogind went away; the session goes on");
                    channel_open = false;
                }
            }
        }
        if stop {
            return end(pid);
        }
    }
}

pub(crate) fn reap(pid: libc::pid_t) -> Option<i32> {
    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == pid {
        Some(status)
    } else if rc < 0 {
        // Already gone (or never ours): nothing to wait for.
        Some(-1)
    } else {
        None
    }
}

/// End the desktop: its whole process group gets `SIGTERM`, then `SIGKILL`.
pub(crate) fn end(pid: libc::pid_t) -> i32 {
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + END_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = reap(pid) {
            return status;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log(WHO, "the session did not end; killing it");
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        status
    }
}

/// A process descriptor for `pid` (`pidfd_open`, Linux 5.3), close-on-exec.
/// Only for a child not yet reaped, whose number cannot have been reused.
pub(crate) fn pidfd_open(pid: libc::pid_t) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        log(
            WHO,
            format_args!(
                "no process descriptor for {pid}: {}",
                io::Error::last_os_error()
            ),
        );
        return None;
    }
    // pidfd_open sets close-on-exec itself.
    Some(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether a `cancel` (or the loss of `reclogind`) is waiting on `channel`.
pub(crate) fn cancel_waiting(channel: &Channel) -> bool {
    while let Ok(true) = channel.wait_readable(0) {
        match receive(channel) {
            Ok(Some(Command::Cancel)) | Ok(None) | Err(_) => return true,
            Ok(Some(other)) => log(WHO, format_args!("unexpected {} ignored", other.kind())),
        }
    }
    false
}

/// The shell that runs the session: the user's, unless it is csh or tcsh
/// (which do not take `-l -c`) or is not a program that exists; then
/// `/bin/sh`.
fn login_shell(shell: &str) -> &str {
    let path = Path::new(shell);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let runnable = std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    if !shell.starts_with('/') || !runnable || matches!(name, "csh" | "tcsh") {
        "/bin/sh"
    } else {
        shell
    }
}

/// One word for the shell, in single quotes: nothing in it is expanded.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The words of a session file's `Exec`, by the Desktop Entry
/// specification: first the escapes of every string value (`\s`, `\n`,
/// `\t`, `\r`, `\\`), then the command line's own rules: words split at
/// blanks, a word in double quotes may hold blanks and `\"`, `` \` ``,
/// `\$`, `\\`; `%%` is `%`, and every field code (`%f`, `%U`, `%i`, ...)
/// is dropped, as a session is started with no files or URLs. `None` when it
/// cannot be read (an unclosed quote) or holds no word.
fn exec_words(exec: &str) -> Option<Vec<String>> {
    let mut value = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            value.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => value.push(' '),
            Some('n') => value.push('\n'),
            Some('t') => value.push('\t'),
            Some('r') => value.push('\r'),
            Some('\\') => value.push('\\'),
            // Not a value escape: left for the command line's rules.
            Some(other) => {
                value.push('\\');
                value.push(other);
            }
            None => value.push('\\'),
        }
    }

    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '`' | '$' | '\\') => word.push(c),
                            other => {
                                word.push('\\');
                                word.push(other);
                            }
                        },
                        other => word.push(other),
                    }
                }
            }
            // Anything else after `%` is a field code: dropped (a word that
            // was only one is gone).
            '%' => {
                if chars.next() == Some('%') {
                    word.push('%');
                    in_word = true;
                }
            }
            other => {
                word.push(other);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
}
