//! Logging in, on `reclogind`'s side: one login at a time.
//!
//! `begin` chooses the session's console and starts a session worker
//! ([`crate::worker`]) for the person; the worker's questions and messages
//! go to the greeter as `prompt` and `message`, the greeter's `answer` (its
//! secret as the raw packet after it) and `cancel` go back. Once its PAM
//! session is open the worker says `ready_to_start`, and `reclogind` lets the
//! greeter go, gives the display up and shows that console, and checks that
//! it is shown. While the session runs no greeter runs. When the worker has
//! exited, `reclogind` shows the login console again and starts a fresh
//! greeter.
//!
//! A worker is never killed to make room: `cancel` asks it to end, it
//! closes what it opened (PAM session, credentials, console) and exits, and
//! until it has, every `begin` is answered with "Try again in a moment.".
//! One that has not exited fifteen seconds after being asked is killed, and
//! `reclogind` gives its console back itself. A worker that goes away while
//! its desktop runs leaves the desktop to `reclogind`, which ends it (its
//! process group, and the X server's) before the login screen comes back.
//! Every `begin` gets exactly one `auth_result`; a `cancel` that ends a
//! login gets it when the worker is gone.
//!
//! How the greeter itself is started, restarted and given up on belongs to
//! [`crate::supervise`]; this module only calls it.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::{Framed, MessageKind, Secret, ToDaemon, ToGreeter, Who};
use reclogin::wire::Channel;

use crate::worker::{self, FromWorker, SessionRef, ToWorker};
use crate::{last, session, sessions, users, Daemon, Next, WHO};

const GUEST_NOT_BUILT: &str = "The guest session is not built yet.";
const NO_SESSION: &str = "No desktop session is installed.";
const SESSION_GONE: &str = "The chosen session is no longer installed.";
const NO_CONSOLE: &str = "There is no free console for the session.";
const BROKEN: &str = "The login could not be completed.";
const TRY_AGAIN: &str = "Try again in a moment.";
const NOT_SHOWN: &str = "The desktop's screen could not be shown.";

/// How long a worker asked to end has before it is killed.
const END_WAIT: Duration = Duration::from_secs(15);
/// How long a desktop left behind has after `SIGTERM` before `SIGKILL`.
const DESKTOP_END: Duration = Duration::from_secs(10);
/// How long the session's console has to become the one shown.
const SHOW_WITHIN: Duration = Duration::from_secs(5);
/// No new worker this soon after a failed or cancelled attempt.
const FAILURE_PAUSE: Duration = Duration::from_secs(1);
const KDGETMODE: libc::c_ulong = 0x4B3B;
const KDSETMODE: libc::c_ulong = 0x4B3A;

/// The environment a worker starts with; PAM's modules read nothing else.
const WORKER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const VT_OPENQRY: libc::c_ulong = 0x5600;
const VT_GETSTATE: libc::c_ulong = 0x5603;

/// The login under way.
#[derive(Default)]
pub struct Login {
    /// The session the greeter chose (`choose_session`), checked at `begin`.
    chosen: Option<String>,
    worker: Option<Worker>,
    next_prompt: u64,
    /// The greeter was let go for a session; a fresh one starts when the
    /// session has ended.
    greeter_away: bool,
    /// The greeter sent `answer` for this prompt: its next packet is the
    /// secret, raw.
    raw_answer: Option<u64>,
    /// The greeter generation `raw_answer` belongs to.
    seen_generation: u64,
    /// When the last attempt failed or was cancelled.
    last_failure: Option<Instant>,
    /// Desktops left by a worker that went away, being ended.
    orphans: Vec<Orphan>,
    /// A line for the next greeter's screen, sent once it has its state.
    notice: Option<String>,
}

struct Worker {
    child: Child,
    /// `None` once the worker closed it.
    channel: Option<Channel>,
    phase: Phase,
    /// The session's console, chosen at `begin`.
    console: u32,
    /// Who and what, for `last.rs` once the session has started.
    user: String,
    session: String,
    /// The id of the question the greeter may answer now.
    prompt: Option<u64>,
    /// Asked to end (by the greeter, or by reclogind): what it still says
    /// is not passed on.
    cancelled: bool,
    /// The greeter's `cancel` ended this login: it gets its `auth_result`
    /// when the worker is gone.
    owe_answer: bool,
    /// PAM accepted the person: the console may have been handed over.
    accepted: bool,
    /// When it was asked to end; killed `END_WAIT` later.
    ending_since: Option<Instant>,
    killed: bool,
    /// `Supervisor::generation` of the greeter that began this login; a
    /// replaced greeter cannot answer its questions.
    generation: u64,
    /// Process groups to end if the worker goes away without closing them.
    x_server: Option<libc::pid_t>,
    desktop: Option<libc::pid_t>,
    /// By when the session's console must be shown (after `ready_to_start`).
    show_by: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// PAM's conversation and the account check.
    Asking,
    /// Accepted; the console is handed over and the PAM session opened.
    Opening,
    /// The session's console is being shown; the desktop starts.
    Starting,
    Running,
    /// The worker said `failed` or `ended`; it exits now.
    Done,
}

/// Process groups of a session whose worker went away, being ended.
struct Orphan {
    groups: Vec<libc::pid_t>,
    console: u32,
    since: Instant,
    killed: bool,
}

impl Login {
    /// The worker's socket for `poll`, or -1 (ignored by `poll`).
    pub fn fd(&self) -> RawFd {
        self.worker
            .as_ref()
            .and_then(|w| w.channel.as_ref())
            .map_or(-1, |c| c.as_raw_fd())
    }

    /// Whether the greeter was let go for a session: its socket is not
    /// read and its exit is expected.
    pub fn greeter_away(&self) -> bool {
        self.greeter_away
    }

    /// Whether the greeter's next packet is a raw secret (`answer` came).
    pub fn expects_raw_answer(&self) -> bool {
        self.raw_answer.is_some()
    }

    /// Whether something here is timed, so the main loop wakes up often:
    /// a worker asked to end or exiting, a console switch being checked, a
    /// desktop left behind being ended.
    pub fn wants_tick(&self) -> bool {
        !self.orphans.is_empty()
            || self.worker.as_ref().is_some_and(|w| {
                w.ending_since.is_some() || w.show_by.is_some() || w.channel.is_none()
            })
    }

    /// The console of the session being started or running.
    fn session_console(&self) -> Option<u32> {
        let w = self.worker.as_ref()?;
        match w.phase {
            Phase::Starting | Phase::Running if !w.cancelled => Some(w.console),
            _ => None,
        }
    }
}

impl Worker {
    fn send(&mut self, message: &ToWorker) {
        let Some(channel) = &self.channel else {
            return;
        };
        if let Err(e) = channel.send(message, &[]) {
            log(
                WHO,
                format_args!("sending {} to the worker: {e}", message.kind()),
            );
        }
    }

    /// `answer`, and the secret as the raw packet after it.
    fn send_answer(&mut self, secret: &Secret) {
        let Some(channel) = &self.channel else {
            return;
        };
        if let Err(e) = channel
            .send(&ToWorker::Answer, &[])
            .and_then(|()| channel.send_secret(secret))
        {
            log(WHO, format_args!("sending answer to the worker: {e}"));
        }
    }

    /// Ask the worker to end; it closes what it opened and exits.
    fn cancel(&mut self) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        self.prompt = None;
        self.show_by = None;
        self.ending_since = Some(Instant::now());
        self.send(&ToWorker::Cancel);
    }

    /// Whether the process has exited (and is reaped).
    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// Its desktop's and X server's process groups that still exist.
    fn groups_left(&self) -> Vec<libc::pid_t> {
        [self.desktop, self.x_server]
            .into_iter()
            .flatten()
            .filter(|&g| group_exists(g))
            .collect()
    }
}

/// Whether process group `group` still has a process.
fn group_exists(group: libc::pid_t) -> bool {
    group > 1
        && (unsafe { libc::kill(-group, 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

fn signal_groups(groups: &[libc::pid_t], signal: libc::c_int) {
    for &group in groups {
        if group > 1 {
            unsafe {
                libc::kill(-group, signal);
            }
        }
    }
}

/// Console `number` given back by `reclogind` after a worker that did not:
/// text mode if it was left in graphics mode (an X server killed), whatever
/// still has it cut off, owner root, group `tty`, mode 0620.
fn reset_console(number: u32) {
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(format!("/dev/tty{number}"));
    let tty = match opened {
        Ok(tty) => tty,
        Err(e) => {
            log(WHO, format_args!("giving console {number} back: {e}"));
            return;
        }
    };
    let fd = tty.as_raw_fd();
    let mut mode: libc::c_int = 0;
    unsafe {
        if libc::ioctl(fd, KDGETMODE as _, &mut mode) == 0 && mode != 0 {
            libc::ioctl(fd, KDSETMODE as _, 0 as libc::c_long);
        }
    }
    session::give_console_back(fd);
    log(WHO, format_args!("console {number} given back to root"));
}

/// Start `reclogind --session-worker` with its socket as descriptor 3.
fn spawn_worker() -> io::Result<(Child, Channel)> {
    let (ours, theirs) = Channel::pair()?;
    let theirs = theirs.into_fd();
    let their_fd = theirs.as_raw_fd();
    let mut command = Command::new("/proc/self/exe");
    command
        .arg(worker::SWITCH)
        .env_clear()
        .env("PATH", WORKER_PATH)
        .stdin(Stdio::null())
        .current_dir("/");
    unsafe {
        command.pre_exec(move || {
            // Only async-signal-safe calls between fork and exec.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            // reclogind gone: the worker ends the login (and its session).
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGTERM as libc::c_ulong,
                0,
                0,
                0,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if their_fd == worker::SOCKET_FD {
                if libc::fcntl(their_fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(their_fd, worker::SOCKET_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(theirs);
    Ok((child, ours))
}

fn open_tty0() -> io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open("/dev/tty0")
}

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

fn vt_state() -> io::Result<VtStat> {
    let tty0 = open_tty0()?;
    let mut state = VtStat::default();
    if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, &mut state) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(state)
}

/// A console for a session: the first one after the login console that no
/// one has open, so that consoles 1 to 6 keep their text logins. The
/// kernel's in-use bits cover consoles 1 to 15; past those the kernel's
/// own choice (`VT_OPENQRY`) is taken when it is after the login console.
fn free_console(login_console: u32) -> io::Result<u32> {
    let state = vt_state()?;
    for n in (login_console + 1)..16 {
        if state.v_state & (1 << n) == 0 {
            return Ok(n);
        }
    }
    let tty0 = open_tty0()?;
    let mut n: libc::c_int = -1;
    if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_OPENQRY as _, &mut n) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if n > login_console as libc::c_int {
        Ok(n as u32)
    } else {
        Err(io::Error::other("no free console after the login console"))
    }
}

/// A typed user name PAM may be given: any account name, root included.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && c != '/' && c != ':')
}

/// A wait status in words, for the log.
fn describe_status(status: i32) -> String {
    if status == -1 {
        "it never ran".into()
    } else if libc::WIFEXITED(status) {
        format!("exit code {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

impl Daemon {
    /// `begin`, `answer`, `cancel` and `choose_session` from the greeter.
    pub(crate) fn on_login_message(&mut self, message: ToDaemon) -> Next {
        match message {
            ToDaemon::Begin { who } => self.begin(who),
            ToDaemon::Answer { prompt_id } => {
                // The secret is the next packet (`on_raw_answer`).
                self.login.raw_answer = Some(prompt_id);
                Next::Continue
            }
            ToDaemon::Cancel => {
                if let Some(w) = self.login.worker.as_mut() {
                    if matches!(w.phase, Phase::Asking | Phase::Opening) && !w.cancelled {
                        w.owe_answer = true;
                        w.cancel();
                    }
                }
                // Otherwise the attempt's `auth_result` was sent already
                // (or is on its way): it is the answer.
                Next::Continue
            }
            ToDaemon::ChooseSession { id } => {
                self.login.chosen = Some(id);
                Next::Continue
            }
            other => {
                log(WHO, format_args!("{} is not a login message", other.kind()));
                Next::Continue
            }
        }
    }

    /// The greeter's packet after `answer`: the secret, raw. It goes to the
    /// worker when it answers the question open now, else it is dropped
    /// (overwritten).
    pub(crate) fn on_raw_answer(&mut self) -> Next {
        let Some(prompt_id) = self.login.raw_answer.take() else {
            return Next::Continue;
        };
        match self.greeter.channel.recv_secret() {
            Ok(Some(secret)) => {
                if let Some(w) = self.login.worker.as_mut() {
                    if w.phase == Phase::Asking && !w.cancelled && w.prompt == Some(prompt_id) {
                        w.prompt = None;
                        w.send_answer(&secret);
                        return Next::Continue;
                    }
                }
                log(WHO, "an answer to no open question was dropped");
                Next::Continue
            }
            Ok(None) => self.greeter_gone("closed its socket"),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.login.raw_answer = Some(prompt_id);
                Next::Continue
            }
            Err(e) => {
                log(WHO, format_args!("the greeter's answer: {e}"));
                Next::Continue
            }
        }
    }

    fn refuse(&mut self, text: &str) -> Next {
        self.send(&ToGreeter::AuthResult {
            ok: false,
            text: text.into(),
        })
    }

    /// A login cannot start now (the last one is still ending, or failed a
    /// moment ago): said, and the attempt answered.
    fn try_again(&mut self) -> Next {
        if let Next::Stop = self.send(&ToGreeter::Message {
            kind: MessageKind::Error,
            text: TRY_AGAIN.into(),
        }) {
            return Next::Stop;
        }
        self.send(&ToGreeter::AuthResult {
            ok: false,
            text: String::new(),
        })
    }

    fn begin(&mut self, who: Who) -> Next {
        let recently_failed = self
            .login
            .last_failure
            .is_some_and(|t| t.elapsed() < FAILURE_PAUSE);
        if self.login.worker.is_some() || !self.login.orphans.is_empty() || recently_failed {
            log(WHO, "begin while the last login is still ending; try again");
            return self.try_again();
        }
        let user = match who {
            Who::User { name } => {
                let listed = users::listable(Path::new("/etc/passwd"), Path::new("/etc/shells"))
                    .iter()
                    .any(|u| u.name == name);
                if !listed {
                    log(WHO, "begin for a user not in the list refused");
                    return self.refuse(worker::AUTH_FAILED);
                }
                name
            }
            Who::TypedUser { name } => {
                if !valid_name(&name) {
                    return self.refuse(worker::AUTH_FAILED);
                }
                name
            }
            Who::Guest => return self.refuse(GUEST_NOT_BUILT),
        };
        let installed = sessions::installed(
            Path::new(sessions::WAYLAND_DIR),
            Path::new(sessions::X11_DIR),
        );
        let session = match &self.login.chosen {
            Some(id) => match installed.iter().find(|s| &s.id == id) {
                Some(s) => s,
                None => return self.refuse(SESSION_GONE),
            },
            None => match installed.first() {
                Some(s) => s,
                None => return self.refuse(NO_SESSION),
            },
        };
        let session = SessionRef {
            id: session.id.clone(),
            kind: session.kind,
        };
        // The console now, so that PAM knows it while authenticating.
        let console = match free_console(self.console.number) {
            Ok(number) => number,
            Err(e) => {
                log(WHO, format_args!("finding a console for the session: {e}"));
                return self.refuse(NO_CONSOLE);
            }
        };
        let (child, channel) = match spawn_worker() {
            Ok(pair) => pair,
            Err(e) => {
                log(WHO, format_args!("starting the session worker: {e}"));
                return self.refuse(BROKEN);
            }
        };
        let mut w = Worker {
            child,
            channel: Some(channel),
            phase: Phase::Asking,
            console,
            user: user.clone(),
            session: session.id.clone(),
            prompt: None,
            cancelled: false,
            owe_answer: false,
            accepted: false,
            ending_since: None,
            killed: false,
            generation: self.supervisor.generation,
            x_server: None,
            desktop: None,
            show_by: None,
        };
        w.send(&ToWorker::Start {
            user,
            session,
            console,
        });
        self.login.worker = Some(w);
        Next::Continue
    }

    /// The worker's socket is readable.
    pub(crate) fn on_worker(&mut self) -> Next {
        let Some(w) = self.login.worker.as_mut() else {
            return Next::Continue;
        };
        let Some(channel) = &w.channel else {
            return Next::Continue;
        };
        match channel.recv::<FromWorker>() {
            Ok(Some((message, _))) => self.on_worker_message(message),
            Ok(None) => {
                // It is exiting: the rest happens when it has (`login_reap`).
                w.channel = None;
                self.login_reap()
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Next::Continue,
            Err(e) => {
                log(WHO, format_args!("from the session worker: {e}"));
                w.cancel();
                Next::Continue
            }
        }
    }

    fn on_worker_message(&mut self, message: FromWorker) -> Next {
        let Some(w) = self.login.worker.as_mut() else {
            return Next::Continue;
        };
        if w.cancelled {
            // Not passed on; only what is needed to clean up after it.
            match message {
                FromWorker::Accepted => w.accepted = true,
                FromWorker::ReadyToStart { x_server, .. } => w.x_server = x_server,
                FromWorker::Started { desktop, .. } => w.desktop = Some(desktop),
                FromWorker::Failed { .. } | FromWorker::Ended { .. } => w.phase = Phase::Done,
                _ => {}
            }
            return Next::Continue;
        }
        match message {
            FromWorker::Prompt { kind, text } => {
                if w.phase != Phase::Asking {
                    log(WHO, "a question from the worker after the account check");
                    w.cancel();
                    return Next::Continue;
                }
                self.login.next_prompt += 1;
                let id = self.login.next_prompt;
                w.prompt = Some(id);
                self.send(&ToGreeter::Prompt { id, kind, text })
            }
            FromWorker::Message { kind, text } => self.send(&ToGreeter::Message { kind, text }),
            FromWorker::Failed { text } => {
                w.phase = Phase::Done;
                self.login.last_failure = Some(Instant::now());
                self.send(&ToGreeter::AuthResult { ok: false, text })
            }
            FromWorker::Accepted => {
                w.accepted = true;
                if w.phase == Phase::Asking {
                    w.phase = Phase::Opening;
                }
                Next::Continue
            }
            FromWorker::ReadyToStart { console, x_server } => {
                w.x_server = x_server;
                if w.phase != Phase::Opening || console != w.console {
                    log(
                        WHO,
                        format_args!(
                            "the worker is ready on console {console}, not its own {}",
                            w.console
                        ),
                    );
                    w.cancel();
                    return self.refuse(BROKEN);
                }
                w.phase = Phase::Starting;
                w.show_by = Some(Instant::now() + SHOW_WITHIN);
                self.let_greeter_go();
                if let Err(e) = self.console.switch_to(console) {
                    log(WHO, format_args!("showing console {console}: {e}"));
                }
                Next::Continue
            }
            FromWorker::Started { console, desktop } => {
                w.desktop = Some(desktop);
                if w.phase == Phase::Starting && console == w.console {
                    w.phase = Phase::Running;
                    w.show_by = None;
                    // Remembered only now that a session really started.
                    let (user, session) = (w.user.clone(), w.session.clone());
                    last::session_started(&user, &session);
                }
                Next::Continue
            }
            FromWorker::Ended { status } => {
                log(
                    WHO,
                    format_args!("the session ended: {}", describe_status(status)),
                );
                w.phase = Phase::Done;
                Next::Continue
            }
        }
    }

    /// The greeter is told it succeeded, paused and stopped; the display
    /// and the input devices are given up. Its exit is marked as expected
    /// before anything is sent, so that one dying right then is not counted.
    fn let_greeter_go(&mut self) {
        self.login.greeter_away = true;
        self.supervisor.stopping(self.greeter.child.id());
        let _ = self.send(&ToGreeter::AuthResult {
            ok: true,
            text: String::new(),
        });
        let _ = self.send(&ToGreeter::Pause);
        self.inputs.revoke_all();
        self.display.give_up();
        self.shown = false;
        self.stop_greeter();
    }

    /// On `SIGCHLD`, a closed worker socket, and each timed turn: if the
    /// worker has exited, finish with it.
    pub(crate) fn login_reap(&mut self) -> Next {
        let exited = self.login.worker.as_mut().is_some_and(Worker::exited);
        if !exited {
            return Next::Continue;
        }
        let w = self.login.worker.take().expect("checked above");
        self.worker_exited(w)
    }

    fn worker_exited(&mut self, w: Worker) -> Next {
        let clean = w.phase == Phase::Done && !w.killed;
        if !clean {
            let left = w.groups_left();
            if !left.is_empty() {
                // Its desktop runs on, unlocked, on another console: end it
                // before the login screen comes back.
                log(
                    WHO,
                    format_args!(
                        "the session worker went away while its session ran; ending the session on console {}",
                        w.console
                    ),
                );
                signal_groups(&left, libc::SIGTERM);
                self.login.orphans.push(Orphan {
                    groups: left,
                    console: w.console,
                    since: Instant::now(),
                    killed: false,
                });
            } else if w.accepted || w.killed {
                reset_console(w.console);
            }
        }
        if w.cancelled || !clean {
            self.login.last_failure = Some(Instant::now());
        }
        if w.owe_answer && !self.login.greeter_away {
            if let Next::Stop = self.send(&ToGreeter::AuthResult {
                ok: false,
                text: String::new(),
            }) {
                return Next::Stop;
            }
        }
        if !w.cancelled && !clean && matches!(w.phase, Phase::Asking | Phase::Opening) {
            log(WHO, "the session worker ended in the middle of a login");
            if !self.login.greeter_away {
                return self.refuse(BROKEN);
            }
        }
        self.maybe_return_to_login()
    }

    /// Back to the login screen once nothing of the last session is left.
    fn maybe_return_to_login(&mut self) -> Next {
        if self.login.worker.is_none() && self.login.orphans.is_empty() {
            self.return_to_login()
        } else {
            Next::Continue
        }
    }

    /// After a session: a fresh greeter, started through the supervisor
    /// (which counts a greeter that fails to start), and the login console
    /// shown. A greeter started while the console is not yet shown gets the
    /// display and the state when it is (`Daemon::finish_handshake`).
    fn return_to_login(&mut self) -> Next {
        if !self.login.greeter_away {
            return Next::Continue;
        }
        self.login.greeter_away = false;
        self.login.chosen = None;
        let shown_now = matches!(vt_state(), Ok(s) if u32::from(s.v_active) == self.console.number);
        if shown_now {
            self.shown = true;
            if let Err(e) = self.display.take() {
                log(WHO, format_args!("taking the display back: {e}"));
            }
        }
        match self.supervisor.launch_until_up(&self.display, self.shown) {
            Ok(fresh) => self.greeter = fresh,
            Err(reason) => self.give_up(&reason),
        }
        if !self.supervisor.handshake_pending {
            if let Next::Stop = self.login_after_handshake() {
                return Next::Stop;
            }
        }
        if !shown_now {
            if let Err(e) = self.console.switch_to(self.console.number) {
                log(WHO, format_args!("showing the login console: {e}"));
            }
        }
        Next::Continue
    }

    /// The greeter has its display and state: a line kept for its screen
    /// (why the last session did not start) is shown now.
    pub(crate) fn login_after_handshake(&mut self) -> Next {
        match self.login.notice.take() {
            Some(text) => self.send(&ToGreeter::AuthResult { ok: false, text }),
            None => Next::Continue,
        }
    }

    /// Called on `ACQUIRE_SIGNAL` once acknowledged. While a session is
    /// being started or runs, no greeter runs to draw on the login console
    /// (and with the console's keyboard off nobody could switch away from
    /// it), so the session's console is shown again. `None`: the usual
    /// handling applies.
    pub(crate) fn login_on_acquire(&mut self) -> Option<Next> {
        if !self.login.greeter_away {
            return None;
        }
        let console = self.login.session_console()?;
        if let Err(e) = self.console.switch_to(console) {
            log(WHO, format_args!("showing console {console} again: {e}"));
        }
        Some(Next::Continue)
    }

    /// Once per turn of the main loop: a login attempt whose greeter was
    /// replaced (it crashed and the supervisor started another) is ended;
    /// a running session is not touched.
    pub(crate) fn login_check_greeter(&mut self) {
        let generation = self.supervisor.generation;
        if self.login.seen_generation != generation {
            self.login.seen_generation = generation;
            self.login.raw_answer = None;
        }
        if let Some(w) = self.login.worker.as_mut() {
            if matches!(w.phase, Phase::Asking | Phase::Opening)
                && !w.cancelled
                && w.generation != generation
            {
                log(WHO, "the greeter was replaced; the login attempt ends");
                w.cancel();
            }
        }
    }

    /// The timed part, run on every turn of the main loop: a worker asked
    /// to end that has not is killed; the session's console must be shown
    /// within `SHOW_WITHIN`; desktops left behind are ended.
    pub(crate) fn login_tick(&mut self) -> Next {
        let now = Instant::now();
        if let Some(w) = self.login.worker.as_mut() {
            if let Some(since) = w.ending_since {
                if !w.killed && now.duration_since(since) >= END_WAIT {
                    log(
                        WHO,
                        format_args!(
                            "THE SESSION WORKER (pid {}) DID NOT END WITHIN {} SECONDS OF BEING ASKED; KILLING IT. Its PAM session may not be closed.",
                            w.child.id(),
                            END_WAIT.as_secs()
                        ),
                    );
                    let _ = w.child.kill();
                    w.killed = true;
                }
            }
            if let Some(by) = w.show_by {
                let active = vt_state().ok().map(|s| u32::from(s.v_active));
                if active == Some(w.console) {
                    w.show_by = None;
                } else if now >= by {
                    log(
                        WHO,
                        format_args!(
                            "console {} was not shown within {} seconds; the session is cancelled",
                            w.console,
                            SHOW_WITHIN.as_secs()
                        ),
                    );
                    self.login.notice = Some(NOT_SHOWN.into());
                    w.cancel();
                }
            }
        }
        if let Next::Stop = self.login_reap() {
            return Next::Stop;
        }

        let mut finished = false;
        let mut i = 0;
        while i < self.login.orphans.len() {
            let orphan = &mut self.login.orphans[i];
            orphan.groups.retain(|&g| group_exists(g));
            if orphan.groups.is_empty() {
                let console = orphan.console;
                self.login.orphans.remove(i);
                log(
                    WHO,
                    format_args!("the session on console {console} has ended"),
                );
                reset_console(console);
                finished = true;
                continue;
            }
            if !orphan.killed && now.duration_since(orphan.since) >= DESKTOP_END {
                log(
                    WHO,
                    format_args!(
                        "the session on console {} did not end; killing it",
                        orphan.console
                    ),
                );
                signal_groups(&orphan.groups, libc::SIGKILL);
                orphan.killed = true;
            }
            i += 1;
        }
        if finished {
            return self.maybe_return_to_login();
        }
        Next::Continue
    }

    /// reclogind is stopping. A login not yet running is asked to end and
    /// waited for (killed after `END_WAIT`). A running session is left to
    /// its worker, which outlives reclogind and closes it by itself.
    /// Desktops left behind by a dead worker are killed now.
    pub(crate) fn end_login(&mut self) {
        for orphan in self.login.orphans.drain(..) {
            signal_groups(&orphan.groups, libc::SIGKILL);
            reset_console(orphan.console);
        }
        let Some(mut w) = self.login.worker.take() else {
            return;
        };
        if w.phase == Phase::Running && !w.cancelled {
            log(
                WHO,
                format_args!(
                    "the session on console {} goes on without reclogind",
                    w.console
                ),
            );
            return;
        }
        w.cancel();
        let deadline = Instant::now() + END_WAIT;
        while !w.exited() {
            if Instant::now() >= deadline {
                log(
                    WHO,
                    format_args!(
                        "THE SESSION WORKER (pid {}) DID NOT END WITHIN {} SECONDS; KILLING IT",
                        w.child.id(),
                        END_WAIT.as_secs()
                    ),
                );
                let _ = w.child.kill();
                let _ = w.child.wait();
                w.killed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if w.killed || w.phase != Phase::Done {
            let left = w.groups_left();
            signal_groups(&left, libc::SIGKILL);
            if w.accepted || w.killed {
                reset_console(w.console);
            }
        }
    }
}
