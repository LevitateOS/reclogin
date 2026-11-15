//! Starting and ending a desktop session, in the session worker, after PAM
//! accepted the person.
//!
//! The order (see `README.md`, "What a login does"): the console is handed
//! to the user, the PAM session is opened with the console and seat in the
//! PAM environment (the login service's PAM module reads them), `reclogind`
//! shows that console, and then the desktop's process is forked: a session
//! of its own with the console as its controlling terminal and standard
//! input, the user's groups and ids, PAM's environment, and the session
//! file's `Exec` run through the user's login shell so their profile is
//! read. When it exits the PAM session is closed and the console given back.
//!
//! Wayland sessions only. X sessions are stage 3: the X server is started
//! for the user on the console between opening the PAM session and running
//! `Exec` (a module `xserver.rs`, called where [`read`] refuses them now).

use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::SessionKind;
use reclogin::wire::Channel;

use crate::pam::{self, Pam};
use crate::sessions::WAYLAND_DIR;
use crate::worker::{next_signal, FromWorker, SessionRef, Talk, ToWorker};

const WHO: &str = "reclogind (session worker)";

const X_NOT_BUILT: &str = "X sessions are not built yet.";
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
        // Stage 3: read from `X11_DIR`; `run` then starts the X server
        // (`xserver.rs`) before `Exec`.
        SessionKind::X11 => return Err(X_NOT_BUILT.into()),
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
    /// Supplementary groups, from the group database.
    pub groups: Vec<libc::gid_t>,
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
        let mut groups = vec![0 as libc::gid_t; 256];
        let mut count = groups.len() as libc::c_int;
        loop {
            let rc = unsafe {
                libc::getgrouplist(c_name.as_ptr(), gid, groups.as_mut_ptr(), &mut count)
            };
            if rc >= 0 {
                groups.truncate(count.max(0) as usize);
                break;
            }
            if count as usize <= groups.len() || groups.len() >= 65536 {
                return Err(io::Error::other("cannot read the account's groups"));
            }
            groups.resize(count as usize, 0);
        }
        Ok(Account {
            name: text(pwd.pw_name),
            uid,
            gid,
            home,
            shell,
            groups,
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
    uid: libc::uid_t,
    gid: libc::gid_t,
    mode: libc::mode_t,
}

impl Tty {
    fn hand_over(number: u32, account: &Account) -> io::Result<Tty> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/dev/tty{number}"))?;
        let fd: OwnedFd = file.into();
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let tty = Tty {
            fd,
            uid: st.st_uid,
            gid: st.st_gid,
            mode: st.st_mode & 0o7777,
        };
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
    /// back to its old owner.
    fn give_back(&self) {
        let fd = self.fd.as_raw_fd();
        unsafe {
            libc::ioctl(fd, TIOCVHANGUP as _, 0 as libc::c_long);
            if libc::fchown(fd, self.uid, self.gid) != 0 || libc::fchmod(fd, self.mode) != 0 {
                log(
                    WHO,
                    format_args!("giving the console back: {}", io::Error::last_os_error()),
                );
            }
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
    let mut rc = pam.set_item(pam::TTY, &format!("tty{console}"));
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

    let rc = pam.setcred(pam::ESTABLISH_CRED);
    if rc != pam::SUCCESS {
        log(WHO, format_args!("credentials: {}", pam.describe(rc)));
        tty.give_back();
        failed();
        return;
    }
    let rc = pam.open_session();
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
        let rc = pam.close_session();
        if rc != pam::SUCCESS {
            log(
                WHO,
                format_args!("closing the session: {}", pam.describe(rc)),
            );
        }
        pam.setcred(pam::DELETE_CRED);
        tty.give_back();
    };

    let prepared = match Prepared::new(pam, account, plan) {
        Ok(prepared) => prepared,
        Err(e) => {
            log(WHO, format_args!("preparing the session: {e}"));
            close(pam);
            failed();
            return;
        }
    };

    // From here the greeter is gone: every end is `Ended`.
    send(&FromWorker::ReadyToStart { console });
    if !wait_until_shown(console, channel) {
        log(WHO, "cancelled before the session started");
        close(pam);
        send(&FromWorker::Ended { status: -1 });
        return;
    }

    let pid = match prepared.spawn(&tty) {
        Ok(pid) => pid,
        Err(e) => {
            log(WHO, format_args!("starting {}: {e}", plan.exec));
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
    send(&FromWorker::Started);

    let status = wait_for_end(pid, channel, signals);
    log(WHO, format_args!("{}'s session ended", account.name));
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
    groups: Vec<libc::gid_t>,
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
        let command = if bus {
            format!("exec {}", plan.exec)
        } else {
            format!("exec dbus-run-session -- {}", plan.exec)
        };

        Ok(Prepared {
            shell: CString::new(account.shell.as_str()).map_err(bad)?,
            argv: vec![
                CString::new(account.shell.as_str()).map_err(bad)?,
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
            groups: account.groups.clone(),
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
                || libc::ioctl(tty_fd, TIOCSCTTY as _, 0 as libc::c_long) < 0
                || libc::dup2(tty_fd, 0) < 0
            {
                libc::_exit(126);
            }
            // Standard output and error stay the worker's: the system log.
            libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
            if libc::setgroups(self.groups.len(), self.groups.as_ptr()) != 0
                || libc::setgid(self.gid) != 0
                || libc::setuid(self.uid) != 0
                || (self.uid != 0 && libc::setuid(0) == 0)
            {
                libc::_exit(126);
            }
            if libc::chdir(self.home.as_ptr()) != 0 {
                libc::chdir(c"/".as_ptr());
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
            Ok(true) => match channel.recv::<ToWorker>() {
                Ok(Some((ToWorker::Cancel, _))) | Ok(None) | Err(_) => return false,
                Ok(Some(_)) => {}
            },
            Ok(false) => {}
            Err(_) => return false,
        }
    }
}

/// Wait until the desktop's process exits; end it when asked to.
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
            match channel.recv::<ToWorker>() {
                Ok(Some((ToWorker::Cancel, _))) => stop = true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    log(WHO, "reclogind went away; ending the session");
                    channel_open = false;
                    stop = true;
                }
            }
        }
        if stop {
            return end(pid);
        }
    }
}

fn reap(pid: libc::pid_t) -> Option<i32> {
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
fn end(pid: libc::pid_t) -> i32 {
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
