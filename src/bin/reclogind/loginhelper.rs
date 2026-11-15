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
//! - Reads, and reports as a [`View`] whenever it changed: the sessions on
//!   `seat0` of class `user`, and which of suspend, restart and power off
//!   the service allows (`yes` only: `challenge`, `no` and `na` hide the
//!   action). Hibernate is never asked.
//! - Re-reads when the service signals (a thread follows every signal under
//!   `/org/freedesktop/login1`: `SessionNew`, `SessionRemoved`, the sessions'
//!   and seat's `PropertiesChanged`, `PrepareForSleep`, and wakes the main
//!   loop; reads follow 150 ms after the last one, so a burst is one read),
//!   on `refresh`, and every five seconds whatever happens.
//! - Performs `ActivateSession`, `Suspend`, `Reboot` and `PowerOff` (never
//!   interactive) on request and answers with `done`.
//! - Without a bus (not up yet, or gone) reports `available: false` and tries
//!   again every two seconds; it never exits for that.
//!
//! Every call and signal here is on the one service, and only the names
//! below are used; nothing is passed to it that came from the greeter except
//! a session id `reclogind` took from an earlier report.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
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
/// No answer to a method call within this: an error, not a hang.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

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
    zbus::blocking::connection::Builder::address(BUS_ADDRESS)?
        .method_timeout(CALL_TIMEOUT)
        .build()
}

fn nonblocking(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(true)
}

fn serve(channel: &Channel) -> io::Result<()> {
    // The signal thread writes a byte here to wake the loop.
    let (mut wake_rx, wake_tx) = UnixStream::pair()?;
    nonblocking(&wake_rx)?;
    nonblocking(&wake_tx)?;

    let mut bus: Option<Connection> = None;
    let mut last: Option<View> = None;
    let mut next_connect = Instant::now();
    let mut next_read = Instant::now();
    let mut settle: Option<Instant> = None;
    // The first report is always sent, and `refresh` forces another.
    let mut force = true;
    let mut said_unreachable = false;

    loop {
        let now = Instant::now();
        if bus.is_none() && now >= next_connect {
            match connect() {
                Ok(connection) => {
                    log(WHO, "connected to the system bus");
                    said_unreachable = false;
                    watch(&connection, wake_tx.try_clone()?);
                    bus = Some(connection);
                    next_read = now;
                }
                Err(e) => {
                    if !said_unreachable {
                        log(WHO, format_args!("the system bus: {e}"));
                        said_unreachable = true;
                    }
                    next_connect = now + RECONNECT;
                }
            }
        }

        let settled = settle.is_some_and(|t| now >= t);
        if force || settled || now >= next_read {
            settle = None;
            force = false;
            next_read = now + REREAD;
            let view = match &bus {
                Some(connection) => match read(connection) {
                    Ok(view) => view,
                    Err(e) => {
                        log(WHO, format_args!("reading the login service: {e}"));
                        bus = None;
                        next_connect = now + RECONNECT;
                        View::default()
                    }
                },
                None => View::default(),
            };
            if last.as_ref() != Some(&view) {
                channel.send(&FromHelper::Report(view.clone()), &[])?;
                last = Some(view);
            }
        }

        // Wait for a request, a signal, or the next due time.
        let mut due = next_read;
        if let Some(t) = settle {
            due = due.min(t);
        }
        if bus.is_none() {
            due = due.min(next_connect);
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
                        let answer = act(&mut bus, &mut next_connect, |c| activate(c, &id));
                        channel.send(&done(request, answer), &[])?;
                        // What the call changed is read right away.
                        settle = Some(Instant::now());
                    }
                    ToHelper::Power { request, action } => {
                        let answer = act(&mut bus, &mut next_connect, |c| power(c, action));
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

/// Run a request on the connection. A refusal by the service (an error
/// reply) is its answer; any other error means the connection is gone, and
/// it is dropped so that the loop makes a new one.
fn act(
    bus: &mut Option<Connection>,
    next_connect: &mut Instant,
    call: impl FnOnce(&Connection) -> zbus::Result<()>,
) -> Result<(), String> {
    let Some(connection) = bus.as_ref() else {
        return Err(NOT_AVAILABLE.into());
    };
    match call(connection) {
        Ok(()) => Ok(()),
        Err(e) => {
            let text = reason(&e);
            if !matches!(e, zbus::Error::MethodError(..)) {
                log(WHO, format_args!("the login service: {e}"));
                *bus = None;
                *next_connect = Instant::now() + RECONNECT;
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

/// A thread that follows every signal under the service's path and wakes
/// the main loop. It ends when its connection does.
fn watch(connection: &Connection, wake: UnixStream) {
    let connection = connection.clone();
    let spawned = std::thread::Builder::new()
        .name("login1-signals".into())
        .spawn(move || {
            use std::io::Write;
            let rule = MatchRule::builder()
                .msg_type(Type::Signal)
                .path_namespace(MANAGER_PATH)
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
    if let Err(e) = spawned {
        // The five-second re-read still keeps the screen right.
        log(WHO, format_args!("starting the signal thread: {e}"));
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
    for (id, uid, user, seat, path) in listed {
        if seat != "seat0" {
            continue;
        }
        let Ok(session) = session_of(connection, id, uid, user, &path) else {
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
    }))
}
