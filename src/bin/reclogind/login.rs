//! Logging in, on `reclogind`'s side: one login at a time.
//!
//! `begin` starts a session worker ([`crate::worker`]) for the person; the
//! worker's questions and messages go to the greeter as `prompt` and
//! `message`, the greeter's `answer` and `cancel` go back. When PAM accepts
//! the person the worker asks for a console; once its PAM session is open
//! it says `ready_to_start`, and `reclogind` lets the greeter go, gives the
//! display up and shows that console. While the session runs no greeter
//! runs. When the worker says `ended`, `reclogind` shows the login console
//! again and starts a fresh greeter.
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
use reclogin::protocol::{Framed, ToDaemon, ToGreeter, Who};
use reclogin::wire::Channel;

use crate::worker::{self, FromWorker, SessionRef, ToWorker};
use crate::{sessions, users, Daemon, Next, WHO};

const GUEST_NOT_BUILT: &str = "The guest session is not built yet.";
const NO_SESSION: &str = "No desktop session is installed.";
const SESSION_GONE: &str = "The chosen session is no longer installed.";
const NO_CONSOLE: &str = "There is no free console for the session.";
const BROKEN: &str = "The login could not be completed.";

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
}

struct Worker {
    child: Child,
    channel: Channel,
    phase: Phase,
    /// The id of the question the greeter may answer now.
    prompt: Option<u64>,
    /// The greeter cancelled or went away: what the worker still says is
    /// not passed on.
    cancelled: bool,
    /// `Supervisor::generation` of the greeter that began this login; a
    /// replaced greeter cannot answer its questions.
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// PAM's conversation, until the worker asks for a console.
    Asking,
    /// Given this console; the PAM session is being opened.
    Console(u32),
    /// The session's console is being shown; the desktop starts.
    Starting(u32),
    Running(u32),
    /// The worker said `failed` or `ended`; it exits now.
    Done,
}

impl Login {
    /// The worker's socket for `poll`, or -1 (ignored by `poll`).
    pub fn fd(&self) -> RawFd {
        self.worker.as_ref().map_or(-1, |w| w.channel.as_raw_fd())
    }

    /// Whether the greeter was let go for a session: its socket is not
    /// read and its exit is expected.
    pub fn greeter_away(&self) -> bool {
        self.greeter_away
    }

    /// The console of the session being started or running.
    fn session_console(&self) -> Option<u32> {
        match self.worker.as_ref()?.phase {
            Phase::Starting(n) | Phase::Running(n) => Some(n),
            _ => None,
        }
    }
}

impl Worker {
    fn send(&mut self, message: &ToWorker) {
        if let Err(e) = self.channel.send(message, &[]) {
            log(
                WHO,
                format_args!("sending {} to the worker: {e}", message.kind()),
            );
        }
    }

    /// Wait at most `timeout` for the worker to exit, then kill it.
    fn finish(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log(WHO, "the session worker did not end; stopping it");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
            ToDaemon::Answer { prompt_id, text } => {
                if let Some(w) = self.login.worker.as_mut() {
                    if w.phase == Phase::Asking && !w.cancelled && w.prompt == Some(prompt_id) {
                        w.prompt = None;
                        w.send(&ToWorker::Answer { secret: text });
                        return Next::Continue;
                    }
                }
                // Not the question asked now: the secret is overwritten as
                // `text` is dropped here.
                log(WHO, "an answer to no open question was dropped");
                Next::Continue
            }
            ToDaemon::Cancel => {
                if let Some(w) = self.login.worker.as_mut() {
                    if matches!(w.phase, Phase::Asking | Phase::Console(_)) && !w.cancelled {
                        w.cancelled = true;
                        w.prompt = None;
                        w.send(&ToWorker::Cancel);
                    }
                }
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

    fn refuse(&mut self, text: &str) -> Next {
        self.send(&ToGreeter::AuthResult {
            ok: false,
            text: text.into(),
        })
    }

    fn begin(&mut self, who: Who) -> Next {
        if let Some(w) = self.login.worker.as_mut() {
            if !w.cancelled {
                log(WHO, "a login is already under way; begin ignored");
                return Next::Continue;
            }
            // A cancelled worker still finishing: it has no session yet.
            let _ = w.child.kill();
            let _ = w.child.wait();
            self.login.worker = None;
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
        let (child, channel) = match spawn_worker() {
            Ok(pair) => pair,
            Err(e) => {
                log(WHO, format_args!("starting the session worker: {e}"));
                return self.refuse(BROKEN);
            }
        };
        let mut w = Worker {
            child,
            channel,
            phase: Phase::Asking,
            prompt: None,
            cancelled: false,
            generation: self.supervisor.generation,
        };
        w.send(&ToWorker::Start { user, session });
        self.login.worker = Some(w);
        Next::Continue
    }

    /// The worker's socket is readable.
    pub(crate) fn on_worker(&mut self) -> Next {
        let Some(w) = self.login.worker.as_mut() else {
            return Next::Continue;
        };
        match w.channel.recv::<FromWorker>() {
            Ok(Some((message, _))) => self.on_worker_message(message),
            Ok(None) => self.worker_gone(),
            Err(e) => {
                log(WHO, format_args!("from the session worker: {e}"));
                if let Some(w) = self.login.worker.as_mut() {
                    w.send(&ToWorker::Cancel);
                }
                self.worker_gone()
            }
        }
    }

    fn on_worker_message(&mut self, message: FromWorker) -> Next {
        let Some(w) = self.login.worker.as_mut() else {
            return Next::Continue;
        };
        if w.cancelled {
            return Next::Continue;
        }
        match message {
            FromWorker::Prompt { kind, text } => {
                self.login.next_prompt += 1;
                let id = self.login.next_prompt;
                w.prompt = Some(id);
                self.send(&ToGreeter::Prompt { id, kind, text })
            }
            FromWorker::Message { kind, text } => self.send(&ToGreeter::Message { kind, text }),
            FromWorker::Failed { text } => {
                w.phase = Phase::Done;
                self.send(&ToGreeter::AuthResult { ok: false, text })
            }
            FromWorker::NeedConsole => match free_console(self.console.number) {
                Ok(number) => {
                    w.phase = Phase::Console(number);
                    w.send(&ToWorker::Console { number });
                    Next::Continue
                }
                Err(e) => {
                    log(WHO, format_args!("finding a console for the session: {e}"));
                    w.cancelled = true;
                    w.send(&ToWorker::Cancel);
                    self.refuse(NO_CONSOLE)
                }
            },
            FromWorker::ReadyToStart { console } => {
                if w.phase != Phase::Console(console) {
                    log(
                        WHO,
                        format_args!("the worker started on console {console}, not its own"),
                    );
                    w.cancelled = true;
                    w.send(&ToWorker::Cancel);
                    return self.refuse(BROKEN);
                }
                w.phase = Phase::Starting(console);
                self.let_greeter_go();
                if let Err(e) = self.console.switch_to(console) {
                    log(WHO, format_args!("showing console {console}: {e}"));
                }
                Next::Continue
            }
            FromWorker::Started => {
                if let Phase::Starting(n) = w.phase {
                    w.phase = Phase::Running(n);
                }
                Next::Continue
            }
            FromWorker::Ended { status } => {
                log(
                    WHO,
                    format_args!("the session ended: {}", describe_status(status)),
                );
                w.phase = Phase::Done;
                self.return_to_login()
            }
        }
    }

    /// The greeter is told it succeeded, paused and stopped; the display
    /// and the input devices are given up.
    fn let_greeter_go(&mut self) {
        let _ = self.send(&ToGreeter::AuthResult {
            ok: true,
            text: String::new(),
        });
        let _ = self.send(&ToGreeter::Pause);
        self.inputs.revoke_all();
        self.display.give_up();
        self.shown = false;
        self.login.greeter_away = true;
        self.stop_greeter();
    }

    /// The worker has exited (its socket closed).
    fn worker_gone(&mut self) -> Next {
        let Some(mut w) = self.login.worker.take() else {
            return Next::Continue;
        };
        w.finish(Duration::from_secs(2));
        if w.cancelled {
            return if self.login.greeter_away {
                self.return_to_login()
            } else {
                Next::Continue
            };
        }
        match w.phase {
            Phase::Asking | Phase::Console(_) => {
                log(WHO, "the session worker ended in the middle of a login");
                self.refuse(BROKEN)
            }
            Phase::Starting(_) | Phase::Running(_) => {
                log(WHO, "the session worker ended without closing its session");
                self.return_to_login()
            }
            Phase::Done => {
                if self.login.greeter_away {
                    self.return_to_login()
                } else {
                    Next::Continue
                }
            }
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
        if !shown_now {
            if let Err(e) = self.console.switch_to(self.console.number) {
                log(WHO, format_args!("showing the login console: {e}"));
            }
        }
        Next::Continue
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
        if let Some(w) = self.login.worker.as_mut() {
            if matches!(w.phase, Phase::Asking | Phase::Console(_))
                && !w.cancelled
                && w.generation != generation
            {
                log(WHO, "the greeter was replaced; the login attempt ends");
                w.cancelled = true;
                w.prompt = None;
                w.send(&ToWorker::Cancel);
            }
        }
    }

    /// reclogind is stopping: end the login or session and wait for the
    /// worker to close it.
    pub(crate) fn end_login(&mut self) {
        if let Some(mut w) = self.login.worker.take() {
            w.send(&ToWorker::Cancel);
            w.finish(Duration::from_secs(15));
        }
    }
}
