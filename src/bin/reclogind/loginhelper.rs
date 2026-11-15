//! The login-service helper: `reclogind --login-service`.
//!
//! The only code that talks to `org.freedesktop.login1` (systemd-logind or
//! elogind) over the system bus. It is a separate process because the D-Bus
//! library starts threads and `reclogind`'s main process must stay
//! single-threaded (it forks the greeter and locks it down between fork and
//! exec). It runs as root with its end of a socket pair as descriptor 3
//! (the crate's own framing, [`reclogin::wire`]), has nothing else to do,
//! and ends when `reclogind` goes (its socket closes, and `reclogind` also
//! asked for `SIGTERM` at that moment).
//!
//! What it does:
//!
//! - Reads, and reports as a [`View`] whenever it changed: every session of
//!   class `user`, on any seat or none (over the network, marked `remote`),
//!   and which of suspend, restart and power off the service allows (`yes`
//!   only: `challenge`, `no` and `na` hide the action). Hibernate is never
//!   asked.
//! - Re-reads when the service signals (a thread follows every signal from
//!   `org.freedesktop.login1` under `/org/freedesktop/login1`: `SessionNew`,
//!   `SessionRemoved`, the sessions' and seat's `PropertiesChanged`,
//!   `PrepareForSleep`, and wakes the main loop; reads follow 150 ms after
//!   the last one, so a burst is one read), on `refresh`, and every five
//!   seconds whatever happens; never more than about three times a second.
//! - Performs `ActivateSession`, `Suspend`, `Reboot` and `PowerOff` (never
//!   interactive) on request and answers with `done`.
//! - One bus connection at a time. An error from the login service itself
//!   (it is not on the bus, a call failed or took too long) keeps the
//!   connection and tries again with a growing pause (one second to thirty).
//!   Only a broken connection is closed, its signal thread waited for, and a
//!   new one made two seconds later. Meanwhile it reports `available:
//!   false`; it never exits for that.
//!
//! Every call and signal here is on the one service, and only the names
//! below are used; nothing is passed to it that came from the greeter except
//! a session id `reclogind` took from an earlier report.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::protocol::{Power, PowerAction};
use reclogin::wire::Channel;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::MatchRule;

use crate::logind::{FromHelper, SeatSession, ToHelper, View, SOCKET_FD};

const WHO: &str = "reclogind (login service)";

/// The system bus, where the try showed it is on both systems (on
/// LevitateOS `/var/run` is not a link to `/run`).
const BUS_ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";
const SERVICE: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// A read at least this often, signals or not.
const REREAD: Duration = Duration::from_secs(5);
/// A read this long after the last signal.
const SETTLE: Duration = Duration::from_millis(150);
/// Another try at the bus this long after a failure.
const RECONNECT: Duration = Duration::from_secs(2);
/// The least time between two reads.
const READ_GAP: Duration = Duration::from_millis(300);
/// After an error of the login service, the next read waits this long at
/// first, twice as long after each further error, at most `SERVICE_PAUSE_MOST`.
const SERVICE_PAUSE_LEAST: Duration = Duration::from_secs(1);
const SERVICE_PAUSE_MOST: Duration = Duration::from_secs(30);
/// How often the end of an old connection's signal thread is looked for.
const THREAD_CHECK: Duration = Duration::from_millis(200);
/// No answer to a method call within this: an error, not a hang.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// The hidden switch of the one-shot `reclogind --end-user UID`.
pub const END_USER: &str = "--end-user";
/// How long that one call may take before it is given up.
const END_USER_TIMEOUT: Duration = Duration::from_secs(4);

const NOT_AVAILABLE: &str = "The login service is not available.";

pub fn run() -> ExitCode {
    let channel = Channel::from_fd(unsafe { OwnedFd::from_raw_fd(SOCKET_FD) });
    match serve(&channel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("{e}"));
            ExitCode::FAILURE
        }
    }
}

fn connect() -> zbus::Result<Connection> {
    connect_with(CALL_TIMEOUT)
}

fn connect_with(timeout: Duration) -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::address(BUS_ADDRESS)?
        .method_timeout(timeout)
        .build()
}

/// `reclogind --end-user UID`: one call, `TerminateUser(u)`, on the system
/// bus, then exit. Run by the guest's cleanup (`guest.rs`), which cannot make
/// the call itself because it must stay single-threaded. The service's
/// answer does not matter to the caller (its own sweep follows); the exit
/// status says whether the call went through.
pub fn end_user() -> ExitCode {
    if unsafe { libc::geteuid() } != 0 {
        log(WHO, "--end-user must run as root");
        return ExitCode::FAILURE;
    }
    let Some(uid) = std::env::args().nth(2).and_then(|a| a.parse::<u32>().ok()) else {
        log(WHO, "--end-user needs a user number");
        return ExitCode::from(2);
    };
    let call = connect_with(END_USER_TIMEOUT)
        .and_then(|connection| manager(&connection)?.call::<_, _, ()>("TerminateUser", &(uid,)));
    match call {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("TerminateUser({uid}): {}", reason(&e)));
            ExitCode::FAILURE
        }
    }
}

fn nonblocking(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(true)
}

/// The one connection to the bus, and the thread following its signals.
struct Bus {
    connection: Connection,
    watch: Option<JoinHandle<()>>,
}

/// The connection's state in the loop.
struct Link {
    bus: Option<Bus>,
    /// The signal thread of a connection that was closed, until it ended;
    /// no new connection is made before it has.
    closing: Option<(JoinHandle<()>, Instant)>,
    said_thread_lingers: bool,
    next_connect: Instant,
    said_unreachable: bool,
    /// After an error of the service: no read or call before this.
    service_pause: Duration,
    retry_at: Option<Instant>,
}

impl Link {
    /// The connection broke: close it (its signal thread ends with it) and
    /// make a new one later.
    fn broken(&mut self) {
        if let Some(bus) = self.bus.take() {
            // Shared by the signal thread's clone: closing it ends that
            // thread's wait as well.
            let _ = bus.connection.close();
            if let Some(watch) = bus.watch {
                self.closing = Some((watch, Instant::now()));
                self.said_thread_lingers = false;
            }
        }
        self.next_connect = Instant::now() + RECONNECT;
        self.retry_at = None;
        self.service_pause = SERVICE_PAUSE_LEAST;
    }

    /// The service answered with an error (or not in time) on a good
    /// connection: keep it, pause before the next try.
    fn service_failed(&mut self) {
        self.retry_at = Some(Instant::now() + self.service_pause);
        self.service_pause = (self.service_pause * 2).min(SERVICE_PAUSE_MOST);
    }

    fn service_answered(&mut self) {
        self.retry_at = None;
        self.service_pause = SERVICE_PAUSE_LEAST;
    }

    /// An error from a call on the connection: the connection is broken
    /// only if zbus says it is closed; anything else is the service's.
    fn failed(&mut self, error: &zbus::Error) {
        let closed = self
            .bus
            .as_ref()
            .is_none_or(|bus| bus.connection.is_closed());
        if closed {
            log(WHO, format_args!("the bus connection broke: {error}"));
            self.broken();
        } else {
            self.service_failed();
        }
    }

    /// Whether an old connection's signal thread still runs (and so no
    /// new connection may be made yet).
    fn old_thread_runs(&mut self) -> bool {
        let Some((watch, since)) = &self.closing else {
            return false;
        };
        if watch.is_finished() {
            if let Some((watch, _)) = self.closing.take() {
                let _ = watch.join();
            }
            return false;
        }
        if !self.said_thread_lingers && since.elapsed() >= Duration::from_secs(5) {
            log(
                WHO,
                "the old bus connection's signal thread has not ended; no new connection until it has",
            );
            self.said_thread_lingers = true;
        }
        true
    }
}

fn serve(channel: &Channel) -> io::Result<()> {
    // The signal thread writes a byte here to wake the loop.
    let (mut wake_rx, wake_tx) = UnixStream::pair()?;
    nonblocking(&wake_rx)?;
    nonblocking(&wake_tx)?;

    let mut link = Link {
        bus: None,
        closing: None,
        said_thread_lingers: false,
        next_connect: Instant::now(),
        said_unreachable: false,
        service_pause: SERVICE_PAUSE_LEAST,
        retry_at: None,
    };
    let mut last: Option<View> = None;
    let mut next_read = Instant::now();
    let mut last_read: Option<Instant> = None;
    let mut settle: Option<Instant> = None;
    // The first report is always sent, and `refresh` forces another.
    let mut force = true;

    loop {
        let now = Instant::now();
        if link.bus.is_none() && now >= link.next_connect && !link.old_thread_runs() {
            match connect() {
                Ok(connection) => {
                    log(WHO, "connected to the system bus");
                    link.said_unreachable = false;
                    let watch = watch(&connection, wake_tx.try_clone()?);
                    link.bus = Some(Bus { connection, watch });
                    link.service_answered();
                    next_read = now;
                }
                Err(e) => {
                    if !link.said_unreachable {
                        log(WHO, format_args!("the system bus: {e}"));
                        link.said_unreachable = true;
                    }
                    link.next_connect = now + RECONNECT;
                }
            }
        }

        // Never more than a few reads a second, and none during a pause
        // after the service failed.
        let wanted = force || settle.is_some_and(|t| now >= t) || now >= next_read;
        let mut not_before = last_read.map_or(now, |t| t + READ_GAP);
        if let Some(t) = link.retry_at {
            not_before = not_before.max(t);
        }
        if wanted && now >= not_before {
            settle = None;
            force = false;
            next_read = now + REREAD;
            last_read = Some(now);
            let result = link.bus.as_ref().map(|bus| read(&bus.connection));
            let view = match result {
                Some(Ok(view)) => {
                    link.service_answered();
                    view
                }
                Some(Err(e)) => {
                    log(WHO, format_args!("reading the login service: {e}"));
                    link.failed(&e);
                    View::default()
                }
                None => View::default(),
            };
            if last.as_ref() != Some(&view) {
                channel.send(&FromHelper::Report(view.clone()), &[])?;
                last = Some(view);
            }
        } else if wanted {
            // Due, but too soon: read as soon as allowed.
            settle = Some(not_before);
        }

        // Wait for a request, a signal, or the next due time.
        let mut due = next_read;
        if let Some(t) = settle {
            due = due.min(t);
        }
        if link.bus.is_none() {
            due = due.min(link.next_connect.max(now));
        }
        if link.closing.is_some() {
            due = due.min(now + THREAD_CHECK);
        }
        let timeout = due
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(5_000) as libc::c_int;
        let mut fds = [
            libc::pollfd {
                fd: channel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.max(1)) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[1].revents != 0 {
            use std::io::Read;
            let mut sink = [0u8; 64];
            while matches!(wake_rx.read(&mut sink), Ok(n) if n > 0) {}
            settle.get_or_insert(Instant::now() + SETTLE);
        }
        if fds[0].revents != 0 {
            match channel.recv::<ToHelper>() {
                // reclogind closed its end: nothing left to do.
                Ok(None) => return Ok(()),
                Ok(Some((request, _))) => match request {
                    ToHelper::Refresh => force = true,
                    ToHelper::Activate { request, id } => {
                        let answer = act(&mut link, |c| activate(c, &id));
                        channel.send(&done(request, answer), &[])?;
                        // What the call changed is read right away.
                        settle = Some(Instant::now());
                    }
                    ToHelper::Power { request, action } => {
                        let answer = act(&mut link, |c| power(c, action));
                        channel.send(&done(request, answer), &[])?;
                        settle = Some(Instant::now());
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }
}

fn done(request: u64, answer: Result<(), String>) -> FromHelper {
    match answer {
        Ok(()) => FromHelper::Done {
            request,
            ok: true,
            text: String::new(),
        },
        Err(text) => FromHelper::Done {
            request,
            ok: false,
            text,
        },
    }
}

/// Run a request on the connection. An error reply is the service's
/// answer, and the connection stays; only a closed connection is replaced.
/// A request is tried even during a pause after a failed read: the person
/// asked for it now.
fn act(link: &mut Link, call: impl FnOnce(&Connection) -> zbus::Result<()>) -> Result<(), String> {
    let Some(bus) = link.bus.as_ref() else {
        return Err(NOT_AVAILABLE.into());
    };
    let result = call(&bus.connection);
    let closed = bus.connection.is_closed();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let text = reason(&e);
            if !matches!(e, zbus::Error::MethodError(..)) {
                log(WHO, format_args!("the login service: {e}"));
            }
            if closed {
                link.broken();
            }
            Err(text)
        }
    }
}

/// The service's own words for a refusal.
fn reason(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, description, _) => match description {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => name.to_string(),
        },
        other => other.to_string(),
    }
}

fn manager(connection: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(connection, SERVICE, MANAGER_PATH, MANAGER)
}

/// `ActivateSession(s)`: bring the session to the front.
fn activate(connection: &Connection, id: &str) -> zbus::Result<()> {
    manager(connection)?.call::<_, _, ()>("ActivateSession", &(id,))
}

/// `Suspend(b)`, `Reboot(b)` or `PowerOff(b)`, interactive = false.
fn power(connection: &Connection, action: PowerAction) -> zbus::Result<()> {
    let method = match action {
        PowerAction::Sleep => "Suspend",
        PowerAction::Restart => "Reboot",
        PowerAction::ShutDown => "PowerOff",
    };
    manager(connection)?.call::<_, _, ()>(method, &(false,))
}

/// A thread that follows every signal the service sends under its path and
/// wakes the main loop. It ends when its connection is closed.
fn watch(connection: &Connection, wake: UnixStream) -> Option<JoinHandle<()>> {
    let connection = connection.clone();
    let spawned = std::thread::Builder::new()
        .name("login1-signals".into())
        .spawn(move || {
            use std::io::Write;
            let rule = MatchRule::builder()
                .msg_type(Type::Signal)
                .sender(SERVICE)
                .and_then(|b| b.path_namespace(MANAGER_PATH))
                .map(|b| b.build());
            let rule = match rule {
                Ok(rule) => rule,
                Err(e) => {
                    log(WHO, format_args!("the signal rule: {e}"));
                    return;
                }
            };
            let signals = match MessageIterator::for_match_rule(rule, &connection, None) {
                Ok(signals) => signals,
                Err(e) => {
                    log(
                        WHO,
                        format_args!("following the login service's signals: {e}"),
                    );
                    return;
                }
            };
            let mut wake = wake;
            for message in signals {
                if message.is_err() {
                    break;
                }
                // A full pipe already holds a wake-up.
                let _ = wake.write(&[1]);
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            // The five-second re-read still keeps the screen right.
            log(WHO, format_args!("starting the signal thread: {e}"));
            None
        }
    }
}

/// One read of everything: `ListSessions`, each session's properties
/// (`GetAll` on `org.freedesktop.login1.Session`), `CanSuspend`, `CanReboot`
/// and `CanPowerOff`. A failure of the list is an error (the connection is
/// not good); a session that vanished between the list and its properties
/// is left out.
fn read(connection: &Connection) -> zbus::Result<View> {
    let manager = manager(connection)?;
    let listed: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &())?;
    let mut sessions = Vec::new();
    // Every seat and none: the power question names people logged in over
    // the network or on another seat too (`logind.rs`, `View::local`).
    for (id, uid, user, seat, path) in listed {
        let Ok(session) = session_of(connection, id, uid, user, seat, &path) else {
            continue;
        };
        if let Some(session) = session {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| {
        a.since
            .unwrap_or(0)
            .cmp(&b.since.unwrap_or(0))
            .then_with(|| a.id.cmp(&b.id))
    });
    let can = |method: &str| {
        manager
            .call::<_, _, String>(method, &())
            .is_ok_and(|answer| answer == "yes")
    };
    let power = Power {
        sleep: can("CanSuspend"),
        restart: can("CanReboot"),
        shut_down: can("CanPowerOff"),
    };
    Ok(View {
        available: true,
        sessions,
        power,
    })
}

/// The session at `path` when it is a running session of class `user`.
fn session_of(
    connection: &Connection,
    id: String,
    uid: u32,
    user: String,
    seat: String,
    path: &OwnedObjectPath,
) -> zbus::Result<Option<SeatSession>> {
    let properties = Proxy::new(connection, SERVICE, path.as_ref(), PROPERTIES)?;
    let all: HashMap<String, OwnedValue> = properties.call("GetAll", &(SESSION,))?;
    let text = |name: &str| -> String {
        all.get(name)
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or("")
            .to_string()
    };
    if text("Class") != "user" {
        return Ok(None);
    }
    let state = text("State");
    if state == "closing" {
        return Ok(None);
    }
    let console = all
        .get("VTNr")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let since = all
        .get("Timestamp")
        .and_then(|v| u64::try_from(v).ok())
        .filter(|&t| t > 0)
        .map(|t| (t / 1_000_000) as i64);
    Ok(Some(SeatSession {
        id,
        user,
        uid,
        console,
        kind: text("Type"),
        state,
        desktop: text("Desktop"),
        since,
        seat,
        remote: all
            .get("Remote")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false),
    }))
}
