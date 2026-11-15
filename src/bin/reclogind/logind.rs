//! Other people and power: who is logged in, returning to a running
//! session, and the power actions, on `reclogind`'s side.
//!
//! The login service (systemd-logind or elogind) is asked through a helper
//! process, `reclogind --login-service` ([`crate::loginhelper`]), because
//! the D-Bus library starts threads and this process must stay
//! single-threaded. The two talk over a socket pair with the crate's framing:
//! the helper sends a [`View`] (the sessions on `seat0` of class `user`, and
//! which power actions are allowed) whenever it changes, and answers
//! requests (`activate`, `power`) with `done`.
//!
//! - **While the helper is gone** (it died and waits out its back-off, or
//!   was never started) the view is empty: no logged-in marks, no power
//!   actions on the screen. Logging in does not use it and works.
//! - **The state** the greeter shows is rebuilt and sent again whenever the
//!   view changes ([`Daemon::view_changed`]).
//! - **Returning** ([`Daemon::on_return_to`]): the helper activates the
//!   user's newest session on `seat0`; once it says so, the greeter is
//!   paused and the display given up exactly as at a session start
//!   ([`Daemon::release_display`], the same steps as when the kernel asks
//!   for the console). The console comes back the usual way (`SIGUSR2`),
//!   and the paused greeter is resumed.
//! - **Power** ([`Daemon::on_power`]): refused unless the service allows
//!   it; a restart or shut down while others are logged in is refused
//!   unless `confirmed`, whatever the greeter says.
//! - **Not stealing the display**: see [`Service::start`] and
//!   [`Daemon::view_changed`] (the foreground session).

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::{Framed, Power, PowerAction, ToGreeter};
use reclogin::wire::Channel;
use serde::{Deserialize, Serialize};

use crate::{console, Daemon, Next, WHO};

/// The hidden switch that makes `reclogind` the login-service helper.
pub const SWITCH: &str = "--login-service";

/// The helper's end of the socket.
pub const SOCKET_FD: i32 = 3;

/// What the helper starts with (it needs nothing but the bus socket).
const HELPER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How long start-up waits for the helper's first report.
const FIRST_REPORT: Duration = Duration::from_secs(2);
/// A helper that lived this long starts its next back-off from the least.
const LIVED_WELL: Duration = Duration::from_secs(60);
const BACK_OFF_LEAST: Duration = Duration::from_secs(1);
const BACK_OFF_MOST: Duration = Duration::from_secs(30);
/// After a return was accepted: how long the login console may stay shown
/// before the greeter takes the display back.
const RETURN_WITHIN: Duration = Duration::from_secs(5);

const SERVICE_GONE: &str = "The login service is not available.";
const TRY_AGAIN: &str = "Try again in a moment.";

// ---------------------------------------------------------------------------
// the messages

/// One running session of class `user` on `seat0`, as the helper read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeatSession {
    pub id: String,
    pub user: String,
    pub uid: u32,
    /// `VTNr`; 0 when the session has no console.
    pub console: u32,
    /// `Type`: `wayland`, `x11`, `tty`, ...
    pub kind: String,
    /// `State`: `online` or `active`.
    pub state: String,
    /// `Desktop`: what the session was registered with (`XDG_SESSION_DESKTOP`).
    pub desktop: String,
    /// `Timestamp`, in seconds since the epoch, when the service gives it.
    pub since: Option<i64>,
}

/// Everything asked of the login service, put together.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// The service answered. When not, nothing else here is known.
    pub available: bool,
    /// Oldest first.
    pub sessions: Vec<SeatSession>,
    pub power: Power,
}

impl View {
    /// The user names with a running session, in the order of first start.
    pub fn users(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for session in &self.sessions {
            if !names.contains(&session.user) {
                names.push(session.user.clone());
            }
        }
        names
    }

    /// The newest session of `user` (the one started last).
    fn newest_of(&self, user: &str) -> Option<&SeatSession> {
        self.sessions
            .iter()
            .filter(|s| s.user == user)
            .max_by_key(|s| s.since.unwrap_or(0))
    }

    fn allows(&self, action: PowerAction) -> bool {
        match action {
            PowerAction::Sleep => self.power.sleep,
            PowerAction::Restart => self.power.restart,
            PowerAction::ShutDown => self.power.shut_down,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHelper {
    /// Read everything again and report it.
    Refresh,
    /// `ActivateSession(id)`. Answered with `done { request }`.
    Activate { request: u64, id: String },
    /// `Suspend`, `Reboot` or `PowerOff`, not interactive.
    Power { request: u64, action: PowerAction },
}

impl Framed for ToHelper {
    fn handles(&self) -> usize {
        0
    }

    fn kind(&self) -> &'static str {
        match self {
            ToHelper::Refresh => "refresh",
            ToHelper::Activate { .. } => "activate",
            ToHelper::Power { .. } => "power",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromHelper {
    /// Sent first and whenever the view changed.
    Report(View),
    /// The answer to a request: `text` is the service's reason when `ok` is
    /// false.
    Done {
        request: u64,
        ok: bool,
        text: String,
    },
}

impl Framed for FromHelper {
    fn handles(&self) -> usize {
        0
    }

    fn kind(&self) -> &'static str {
        match self {
            FromHelper::Report(_) => "report",
            FromHelper::Done { .. } => "done",
        }
    }
}

// ---------------------------------------------------------------------------
// the helper, as the main process sees it

struct Helper {
    child: Child,
    /// reclogind's end, non-blocking.
    channel: Channel,
    since: Instant,
}

/// The request waiting for the helper's `done`. One at a time.
enum Pending {
    Activate { request: u64, console: u32 },
    Power { request: u64, action: PowerAction },
}

/// What came from the helper.
enum Received {
    Nothing,
    /// A report; whether it differs from the view held.
    Report(bool),
    Done {
        request: u64,
        ok: bool,
        text: String,
    },
    Gone,
}

pub struct Service {
    helper: Option<Helper>,
    /// What the screen shows; empty while no helper reports.
    pub view: View,
    restart_at: Option<Instant>,
    back_off: Duration,
    next_request: u64,
    pending: Option<Pending>,
    /// The console of a user session that is the one shown while the login
    /// console is not: one returned to, or the one that was shown when
    /// reclogind started. When that session ends and nothing else is shown,
    /// the login console is shown.
    pub foreground: Option<u32>,
    /// After a return was accepted: when to check that the login console is
    /// no longer the one shown.
    returning: Option<Instant>,
    /// A restart or shut down went through: nothing more is asked.
    going_down: bool,
}

impl Service {
    pub fn new() -> Service {
        Service {
            helper: None,
            view: View::default(),
            restart_at: None,
            back_off: BACK_OFF_LEAST,
            next_request: 0,
            pending: None,
            foreground: None,
            returning: None,
            going_down: false,
        }
    }

    /// Start the helper and wait (at most two seconds) for its first report,
    /// so that the console shown at start-up can be judged: a console that
    /// belongs to a running user session is not taken from it. Without a
    /// report (no helper, no bus yet) the view is empty and the login
    /// console is shown as it always was.
    pub fn start() -> Service {
        let mut service = Service::new();
        service.spawn();
        service.wait_first_report();
        service
    }

    /// Whether `console` is the console of a running user session.
    pub fn session_on(&self, console: u32) -> bool {
        console != 0 && self.view.sessions.iter().any(|s| s.console == console)
    }

    fn spawn(&mut self) {
        match spawn_helper() {
            Ok((child, channel)) => {
                if let Err(e) = channel.set_nonblocking() {
                    log(WHO, format_args!("the login-service helper's socket: {e}"));
                }
                self.restart_at = None;
                self.helper = Some(Helper {
                    child,
                    channel,
                    since: Instant::now(),
                });
            }
            Err(e) => {
                log(WHO, format_args!("starting the login-service helper: {e}"));
                self.schedule_restart(Duration::ZERO);
            }
        }
    }

    fn wait_first_report(&mut self) {
        let deadline = Instant::now() + FIRST_REPORT;
        loop {
            let Some(helper) = &self.helper else {
                return;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                log(WHO, "no report from the login service yet; going on");
                return;
            }
            match helper.channel.wait_readable(left.as_millis() as i32 + 1) {
                Ok(true) => match self.receive() {
                    Received::Report(_) => return,
                    Received::Gone => {
                        self.lost();
                        return;
                    }
                    _ => {}
                },
                _ => return,
            }
        }
    }

    /// The helper's socket for `poll`, or -1 (ignored by `poll`).
    pub fn fd(&self) -> RawFd {
        self.helper.as_ref().map_or(-1, |h| h.channel.as_raw_fd())
    }

    /// How long `poll` may wait for something timed here, in milliseconds.
    pub fn wait_ms(&self) -> Option<libc::c_int> {
        let now = Instant::now();
        let mut at: Option<Instant> = None;
        if self.helper.is_none() {
            at = self.restart_at;
        }
        if let Some(t) = self.returning {
            at = Some(at.map_or(t, |a| a.min(t)));
        }
        at.map(|t| {
            (t.saturating_duration_since(now).as_millis().min(60_000) as libc::c_int).max(10)
        })
    }

    fn send(&mut self, message: &ToHelper) -> bool {
        let Some(helper) = &self.helper else {
            return false;
        };
        match helper.channel.send(message, &[]) {
            Ok(()) => true,
            Err(e) => {
                log(
                    WHO,
                    format_args!(
                        "sending {} to the login-service helper: {e}",
                        message.kind()
                    ),
                );
                false
            }
        }
    }

    /// Ask for a fresh report.
    pub fn refresh(&mut self) {
        self.send(&ToHelper::Refresh);
    }

    fn receive(&mut self) -> Received {
        let Some(helper) = &self.helper else {
            return Received::Nothing;
        };
        match helper.channel.recv::<FromHelper>() {
            Ok(Some((FromHelper::Report(view), _))) => {
                let changed = view != self.view;
                self.view = view;
                Received::Report(changed)
            }
            Ok(Some((FromHelper::Done { request, ok, text }, _))) => {
                Received::Done { request, ok, text }
            }
            Ok(None) => Received::Gone,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Received::Nothing,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                log(WHO, format_args!("from the login-service helper: {e}"));
                Received::Nothing
            }
            Err(e) => {
                log(WHO, format_args!("from the login-service helper: {e}"));
                Received::Gone
            }
        }
    }

    fn schedule_restart(&mut self, lived: Duration) {
        let wait = if lived >= LIVED_WELL {
            self.back_off = BACK_OFF_LEAST;
            BACK_OFF_LEAST
        } else {
            self.back_off
        };
        self.back_off = (self.back_off * 2).min(BACK_OFF_MOST);
        self.restart_at = Some(Instant::now() + wait);
        log(
            WHO,
            format_args!(
                "the login-service helper starts again in {} s; no logged-in marks or power actions until it reports",
                wait.as_secs().max(1)
            ),
        );
    }

    /// The helper is gone: reaped, a restart planned, the view emptied.
    /// Returns whether the view changed.
    fn lost(&mut self) -> bool {
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.child.kill();
            let _ = helper.child.wait();
            self.schedule_restart(helper.since.elapsed());
        }
        let changed = self.view != View::default();
        self.view = View::default();
        changed
    }

    /// Whether the helper process has exited.
    fn exited(&mut self) -> bool {
        self.helper
            .as_mut()
            .is_some_and(|h| !matches!(h.child.try_wait(), Ok(None)))
    }

    /// reclogind is stopping.
    pub fn stop(&mut self) {
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.child.kill();
            let _ = helper.child.wait();
        }
    }
}

/// Start `reclogind --login-service` with its socket as descriptor 3.
fn spawn_helper() -> io::Result<(Child, Channel)> {
    let (ours, theirs) = Channel::pair()?;
    let theirs = theirs.into_fd();
    let their_fd = theirs.as_raw_fd();
    let mut command = Command::new("/proc/self/exe");
    command
        .arg(SWITCH)
        .env_clear()
        .env("PATH", HELPER_PATH)
        .stdin(Stdio::null())
        .current_dir("/");
    unsafe {
        command.pre_exec(move || {
            // Only async-signal-safe calls between fork and exec.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            // reclogind gone: the helper goes too.
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
            if their_fd == SOCKET_FD {
                if libc::fcntl(their_fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(their_fd, SOCKET_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(theirs);
    Ok((child, ours))
}

// ---------------------------------------------------------------------------
// what the daemon does with it

impl Daemon {
    /// The helper's socket is readable.
    pub(crate) fn on_logind(&mut self) -> Next {
        loop {
            match self.logind.receive() {
                Received::Nothing => return Next::Continue,
                Received::Report(changed) => {
                    if changed {
                        if let Next::Stop = self.view_changed() {
                            return Next::Stop;
                        }
                    }
                }
                Received::Done { request, ok, text } => {
                    if let Next::Stop = self.on_done(request, ok, text) {
                        return Next::Stop;
                    }
                }
                Received::Gone => return self.logind_lost(),
            }
        }
    }

    /// The timed part, on every turn of the main loop: a helper that
    /// exited is noted and started again after its back-off; a return that
    /// was accepted but did not take the display away is undone.
    pub(crate) fn logind_tick(&mut self) -> Next {
        if self.logind.exited() {
            if let Next::Stop = self.logind_lost() {
                return Next::Stop;
            }
        }
        let now = Instant::now();
        if self.logind.helper.is_none() && self.logind.restart_at.is_some_and(|t| now >= t) {
            self.logind.spawn();
        }
        if self.logind.returning.is_some_and(|t| now >= t) {
            self.logind.returning = None;
            self.logind.foreground = None;
            if !self.shown && console::active_console() == Some(self.console.number) {
                log(
                    WHO,
                    "the session did not come to the front; the login screen stays",
                );
                if let Next::Stop = self.retake_display() {
                    return Next::Stop;
                }
                return self.send(&ToGreeter::AuthResult {
                    ok: false,
                    text: "The session did not come to the front.".into(),
                });
            }
        }
        Next::Continue
    }

    /// The login console is shown again (`SIGUSR2`): nothing is being
    /// returned to or waited for, and the list is read again.
    pub(crate) fn logind_shown(&mut self) {
        self.logind.foreground = None;
        self.logind.returning = None;
        self.logind.refresh();
    }

    /// The helper is gone: no marks and no power actions until it is back;
    /// a request waiting for it fails.
    fn logind_lost(&mut self) -> Next {
        let changed = self.logind.lost();
        if let Some(pending) = self.logind.pending.take() {
            if let Next::Stop = self.request_failed(pending, SERVICE_GONE) {
                return Next::Stop;
            }
        }
        if changed {
            self.view_changed()
        } else {
            Next::Continue
        }
    }

    /// The view changed: reclogind's copy for new greeters, the greeter
    /// itself, and the foreground session.
    fn view_changed(&mut self) -> Next {
        self.supervisor.view = self.logind.view.clone();
        if !self.login.greeter_away() && !self.supervisor.handshake_pending {
            // A paused greeter takes the new state too; it draws it when
            // shown. A greeter started while the console was not shown gets
            // a fresh state with its display (`finish_handshake`).
            let state = self.supervisor.state();
            if let Next::Stop = self.send(&ToGreeter::State(Box::new(state))) {
                return Next::Stop;
            }
        }
        // The session in front (one returned to, or the one shown at start)
        // ended, and the login console is not shown: if nothing else took
        // its place, the login console is shown. A person on another
        // console (a text login, say) is left where they are.
        if let Some(shown) = self.logind.foreground {
            let view = &self.logind.view;
            if view.available && !view.sessions.iter().any(|s| s.console == shown) {
                self.logind.foreground = None;
                if !self.shown
                    && !self.login.greeter_away()
                    && console::active_console() == Some(shown)
                {
                    log(
                        WHO,
                        format_args!(
                            "the session on console {shown} ended; showing the login console"
                        ),
                    );
                    if let Err(e) = self.console.switch_to(self.console.number) {
                        log(WHO, format_args!("showing the login console: {e}"));
                    }
                }
            }
        }
        Next::Continue
    }

    /// The greeter paused, the inputs revoked and the display given up:
    /// what happens when the kernel asks for the console, and when a
    /// session is returned to.
    pub(crate) fn release_display(&mut self) {
        self.shown = false;
        self.greeter.stop_pinging();
        self.inputs.revoke_all();
        self.display.give_up();
        let _ = self.send(&ToGreeter::Pause);
    }

    /// The display taken back and the paused greeter resumed.
    fn retake_display(&mut self) -> Next {
        if self.shown {
            return Next::Continue;
        }
        self.shown = true;
        if let Err(e) = self.display.take() {
            log(WHO, format_args!("taking the display back: {e}"));
        }
        self.send(&ToGreeter::Resume)
    }

    /// `return_to { user }`: bring the user's session to the front.
    pub(crate) fn on_return_to(&mut self, user: String) -> Next {
        let refuse = |this: &mut Daemon, text: &str| {
            this.send(&ToGreeter::AuthResult {
                ok: false,
                text: text.into(),
            })
        };
        if self.login.worker_busy() {
            return refuse(self, TRY_AGAIN);
        }
        if !self.logind.view.available {
            return refuse(self, SERVICE_GONE);
        }
        if self.logind.pending.is_some() {
            return refuse(self, TRY_AGAIN);
        }
        let Some(session) = self.logind.view.newest_of(&user).cloned() else {
            // It ended since the screen was drawn: the next state says so.
            return refuse(self, "That session is not running any more.");
        };
        if session.console == 0 || session.console == self.console.number {
            return refuse(self, "That session has no screen of its own to return to.");
        }
        self.logind.next_request += 1;
        let request = self.logind.next_request;
        self.logind.pending = Some(Pending::Activate {
            request,
            console: session.console,
        });
        let sent = self.logind.send(&ToHelper::Activate {
            request,
            id: session.id,
        });
        if sent {
            Next::Continue
        } else {
            self.logind.pending = None;
            refuse(self, SERVICE_GONE)
        }
    }

    /// `power { action, confirmed }` from the greeter.
    pub(crate) fn on_power(&mut self, action: PowerAction, confirmed: bool) -> Next {
        let refuse = |this: &mut Daemon, text: &str| {
            this.send(&ToGreeter::PowerRefused { text: text.into() })
        };
        if self.logind.going_down {
            return Next::Continue;
        }
        if !self.logind.view.available {
            return refuse(self, SERVICE_GONE);
        }
        if !self.logind.view.allows(action) {
            return refuse(self, "The system does not allow that now.");
        }
        // Restart and shut down end other people's sessions: only after the
        // question, whatever the greeter says. Sleep keeps them.
        if matches!(action, PowerAction::Restart | PowerAction::ShutDown)
            && !confirmed
            && !self.logind.view.users().is_empty()
        {
            return refuse(
                self,
                "Other people are logged in. Choose it again to see who.",
            );
        }
        if self.logind.pending.is_some() {
            return refuse(self, TRY_AGAIN);
        }
        self.logind.next_request += 1;
        let request = self.logind.next_request;
        self.logind.pending = Some(Pending::Power { request, action });
        log(
            WHO,
            format_args!("asking the login service to {}", verb(action)),
        );
        if self.logind.send(&ToHelper::Power { request, action }) {
            Next::Continue
        } else {
            self.logind.pending = None;
            refuse(self, SERVICE_GONE)
        }
    }

    /// The helper answered a request.
    fn on_done(&mut self, request: u64, ok: bool, text: String) -> Next {
        let matches = match &self.logind.pending {
            Some(Pending::Activate { request: r, .. })
            | Some(Pending::Power { request: r, .. }) => *r == request,
            None => false,
        };
        if !matches {
            return Next::Continue;
        }
        let pending = self.logind.pending.take().expect("checked above");
        if !ok {
            let reason = if text.is_empty() {
                "No reason was given.".to_string()
            } else {
                text
            };
            return self.request_failed(pending, &reason);
        }
        match pending {
            Pending::Activate { console, .. } => {
                // The session's console comes to the front; the display goes
                // with it as at a session start. The kernel may have asked
                // for the console already: then this does nothing again.
                self.logind.foreground = Some(console);
                self.logind.returning = Some(Instant::now() + RETURN_WITHIN);
                let ok = self.send(&ToGreeter::AuthResult {
                    ok: true,
                    text: String::new(),
                });
                if let Next::Stop = ok {
                    return Next::Stop;
                }
                if self.shown {
                    self.release_display();
                }
                Next::Continue
            }
            Pending::Power { action, .. } => {
                log(WHO, format_args!("the login service will {}", verb(action)));
                if action != PowerAction::Sleep {
                    self.logind.going_down = true;
                }
                Next::Continue
            }
        }
    }

    /// A request failed (the service refused, or the helper went): the
    /// greeter hears why, and a display already given up is taken back.
    fn request_failed(&mut self, pending: Pending, reason: &str) -> Next {
        match pending {
            Pending::Activate { .. } => {
                log(
                    WHO,
                    format_args!("returning to the session failed: {reason}"),
                );
                self.logind.foreground = None;
                self.logind.returning = None;
                if !self.shown && console::active_console() == Some(self.console.number) {
                    if let Next::Stop = self.retake_display() {
                        return Next::Stop;
                    }
                }
                self.send(&ToGreeter::AuthResult {
                    ok: false,
                    text: format!("The session could not be brought to the front. {reason}"),
                })
            }
            Pending::Power { action, .. } => {
                log(WHO, format_args!("{} failed: {reason}", verb(action)));
                self.send(&ToGreeter::PowerRefused {
                    text: format!("Could not {}. {reason}", verb(action)),
                })
            }
        }
    }
}

fn verb(action: PowerAction) -> &'static str {
    match action {
        PowerAction::Sleep => "sleep",
        PowerAction::Restart => "restart",
        PowerAction::ShutDown => "shut down",
    }
}
