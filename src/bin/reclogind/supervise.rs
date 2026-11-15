//! Keeping the greeter running, giving up safely, and putting the console
//! back whatever happens to reclogind.
//!
//! - A greeter that exits without being asked to is replaced at once. One
//!   that cannot even be started (missing program, missing account, an
//!   unreachable socket) counts the same way.
//! - A greeter that has its display and state but is not `ready` within a
//!   minute, or once ready stops answering pings, is hung and counted the
//!   same way (`greeter.rs`). One that cannot set a mode says so in its exit
//!   status (`GREETER_EXIT_NO_MODE`), which the log names. A message
//!   reclogind cannot encode is its own fault and never counted.
//! - Three such failures within sixty seconds: reclogind gives up. It says
//!   why in the system log and on standard error, gives the display up,
//!   restores the console, writes one line on console 1, shows console 1
//!   (where a text login waits) and exits with [`GAVE_UP`].
//! - A greeter that goes because reclogind asked it to (`quit`, a session
//!   taking over) never counts: whoever stops it calls
//!   [`Supervisor::stopping`] with its process id (`Daemon::stop_greeter`
//!   does). A greeter started later, by any path, is supervised again.
//! - Starting a greeter after one was stopped on purpose (a session ended):
//!   [`Supervisor::launch_until_up`] starts it, sends `hello`, the display
//!   and the state, and counts failures; `greeter::start` alone only
//!   starts the process.
//! - The last resort: whatever ends reclogind (a panic, a fatal signal, an
//!   early exit that skips the normal way out), the console goes back to
//!   text mode with its keyboard. A panic hook and handlers for the fatal
//!   signals restore it from numbers recorded when the console was taken
//!   ([`arm_last_resort`]). Every other signal that would end it by default
//!   gets a handler that does nothing ([`handle_stray_signals`]). SIGKILL
//!   cannot be caught: for that both init services run
//!   `reclogind --reset-console 7` after reclogind has gone (OpenRC through
//!   `dist/openrc/reclogind-run`).

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use reclogin::config::Config;
use reclogin::log;
use reclogin::protocol::{ToGreeter, GREETER_EXIT_NO_MODE, VERSION};

use crate::console::{self, Console};
use crate::display::Display;
use crate::greeter::{self, Greeter};
use crate::inputs::Inputs;
use crate::{Daemon, Next, WHO};

/// reclogind's exit status after giving up. The init services do not
/// start it again on this status.
pub const GAVE_UP: u8 = 3;
/// Failures within [`WINDOW`] that make reclogind give up.
const FAILURES: usize = 3;
const WINDOW: Duration = Duration::from_secs(60);
/// Where the text login waits after giving up.
const FALLBACK_CONSOLE: u32 = 1;

/// Starts greeters and counts the ones that fail.
pub struct Supervisor {
    program: PathBuf,
    account: String,
    brand_dir: PathBuf,
    config: Config,
    /// When each recent failure happened, oldest first.
    failures: VecDeque<Instant>,
    /// What could not be found when the last start failed; cleared by a
    /// greeter that started.
    missing: Option<String>,
    /// The process id of the greeter reclogind made go; its exit is not a
    /// failure. Set through [`Supervisor::stopping`] by whoever stops the
    /// greeter. Any other greeter, however it was started, is supervised.
    stopped: Option<u32>,
    /// The greeter was started while the login console was not shown: it
    /// has `hello` and waits for the display and the state, which
    /// [`Daemon::finish_handshake`] sends when the console is shown again.
    pub handshake_pending: bool,
    /// Counts greeters started, so that a loop can tell that the greeter
    /// (and its socket) was replaced under it.
    pub generation: u64,
    /// What the login service last reported (`logind.rs`): who is logged
    /// in and which power actions are allowed, for every state sent.
    pub view: crate::logind::View,
}

impl Supervisor {
    pub fn new(program: PathBuf, account: String, brand_dir: PathBuf, config: Config) -> Self {
        Supervisor {
            program,
            account,
            brand_dir,
            config,
            failures: VecDeque::new(),
            missing: None,
            stopped: None,
            handshake_pending: false,
            generation: 0,
            view: Default::default(),
        }
    }

    /// The state a greeter shows now.
    pub fn state(&self) -> reclogin::protocol::State {
        crate::state::read(&self.config, &self.view)
    }

    /// reclogind is about to make the greeter with process id `pid` go
    /// (`quit`, a session taking over): its exit is not a failure.
    pub fn stopping(&mut self, pid: u32) {
        self.stopped = Some(pid);
    }

    /// What kept the last greeter from starting at all, if the last
    /// attempt failed that way.
    pub fn missing(&self) -> Option<&str> {
        self.missing.as_deref()
    }

    /// Note one failure. Returns the reason to give up with when it is the
    /// third within a minute.
    fn record(&mut self, what: &str) -> Option<String> {
        let now = Instant::now();
        while self
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) >= WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        let count = self.failures.len();
        log(
            WHO,
            format_args!("the greeter {what} ({count} of {FAILURES} within a minute)"),
        );
        (count >= FAILURES).then(|| what.to_string())
    }

    /// Start one greeter and send it what it needs first: `hello`, and when
    /// the console is shown also the display and the state.
    fn launch(&mut self, display: &Display, shown: bool) -> Result<Greeter, String> {
        let mut greeter = greeter::start(&self.program, &self.account).map_err(|e| {
            let text = format!("{e:#}");
            self.missing = Some(text.clone());
            format!("could not be started: {text}")
        })?;
        self.missing = None;
        self.stopped = None;
        self.handshake_pending = false;
        let sent = greeter
            .send(
                &ToGreeter::Hello {
                    version: VERSION,
                    brand_dir: self.brand_dir.clone(),
                },
                &[],
            )
            .and_then(|()| {
                if shown {
                    self.send_display_and_state(&mut greeter, display)
                } else {
                    self.handshake_pending = true;
                    Ok(())
                }
            });
        if let Err(e) = sent {
            let _ = greeter.child.kill();
            let _ = greeter.child.wait();
            return Err(format!("could not be reached after starting: {e}"));
        }
        self.generation += 1;
        Ok(greeter)
    }

    fn send_display_and_state(
        &self,
        greeter: &mut Greeter,
        display: &Display,
    ) -> std::io::Result<()> {
        let state = self.state();
        log(
            WHO,
            format_args!(
                "{} users, {} sessions, layout {}",
                state.users.len(),
                state.sessions.len(),
                state.current_layout
            ),
        );
        greeter.send(
            &ToGreeter::Display {
                path: display.path.clone(),
            },
            &[display.as_fd()],
        )?;
        greeter.send(&ToGreeter::State(Box::new(state)), &[])?;
        // Its minute to get ready starts (`greeter.rs`).
        greeter.handshake_done();
        Ok(())
    }

    /// Start greeters until one is up, counting each one that is not.
    /// `Err` is the reason to give up with.
    pub fn launch_until_up(&mut self, display: &Display, shown: bool) -> Result<Greeter, String> {
        loop {
            match self.launch(display, shown) {
                Ok(greeter) => return Ok(greeter),
                Err(what) => {
                    if let Some(reason) = self.record(&what) {
                        return Err(reason);
                    }
                }
            }
        }
    }
}

impl Daemon {
    /// The greeter exited, closed its socket or cannot be reached. Unless
    /// reclogind made it go, that is a failure: start a new one, or give up
    /// after the third within a minute.
    pub(crate) fn greeter_gone(&mut self, what: &str) -> Next {
        if self.supervisor.stopped == Some(self.greeter.child.id()) {
            return Next::Continue;
        }
        // Make sure the old one is gone (it may still run with its socket
        // closed) and reaped. Both do nothing to one already reaped.
        let _ = self.greeter.child.kill();
        let status = match self.greeter.child.wait() {
            // Its own word that the display took no mode (`kms.rs`).
            Ok(status) if status.code() == Some(GREETER_EXIT_NO_MODE.into()) => {
                format!("{status}: it could not set a mode on {}", self.display.path)
            }
            Ok(status) => status.to_string(),
            Err(e) => format!("not reaped: {e}"),
        };
        self.inputs.revoke_all();
        let what = format!("{what} ({status})");
        if let Some(reason) = self.supervisor.record(&what) {
            self.give_up(&reason);
        }
        match self.supervisor.launch_until_up(&self.display, self.shown) {
            Ok(greeter) => {
                self.greeter = greeter;
                Next::Continue
            }
            Err(reason) => self.give_up(&reason),
        }
    }

    /// The login console is shown again and the greeter, started while it
    /// was not, still waits for the display and the state.
    pub(crate) fn finish_handshake(&mut self) -> Next {
        self.supervisor.handshake_pending = false;
        match self
            .supervisor
            .send_display_and_state(&mut self.greeter, &self.display)
        {
            // A line kept for the screen (`login.rs`) goes after the state.
            Ok(()) => self.login_after_handshake(),
            Err(e) => self.greeter_gone(&format!("could not be reached: {e}")),
        }
    }

    /// Stop trying: see [`give_up`].
    pub(crate) fn give_up(&mut self, reason: &str) -> ! {
        self.supervisor.stopping(self.greeter.child.id());
        let _ = self.greeter.child.kill();
        let _ = self.greeter.child.wait();
        let missing = self.supervisor.missing().map(str::to_string);
        give_up(
            &mut self.console,
            &self.display,
            &mut self.inputs,
            reason,
            missing.as_deref(),
        )
    }
}

/// Give up: say why in the system log and on standard error, give the
/// devices up, restore the console, write the line on console 1, show
/// console 1, and exit with [`GAVE_UP`]. The greeter must already be gone.
/// `missing` is what kept the last greeter from starting at all, if that
/// was the last failure; it is named on console 1 too.
pub fn give_up(
    console: &mut Console,
    display: &Display,
    inputs: &mut Inputs,
    reason: &str,
    missing: Option<&str>,
) -> ! {
    let text = format!("the login screen failed three times within a minute, giving up: {reason}");
    log(WHO, &text);
    system_log(&text);
    inputs.revoke_all();
    display.give_up();
    console.previous = FALLBACK_CONSOLE;
    console.restore();
    let line = match missing {
        Some(missing) => format!(
            "The login screen stopped: it failed to start three times within a minute ({missing}). Log in here."
        ),
        None => "The login screen stopped: it failed to start three times within a minute. Log in here."
            .to_string(),
    };
    if let Err(e) = write_on_console(FALLBACK_CONSOLE, &line) {
        log(
            WHO,
            format_args!("writing on console {FALLBACK_CONSOLE}: {e}"),
        );
    }
    std::process::exit(GAVE_UP.into())
}

/// One line on console `number`, starting on a fresh line and ending with
/// one, so that the text login's prompt above it stays readable and typing
/// starts below it.
fn write_on_console(number: u32, line: &str) -> std::io::Result<()> {
    let mut tty = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/dev/tty{number}"))?;
    tty.write_all(format!("\r\n{line}\r\n").as_bytes())
}

/// The system log (syslog), as well as standard error: under OpenRC
/// standard error may go nowhere a person looks.
fn system_log(text: &str) {
    let Ok(text) = CString::new(text) else {
        return;
    };
    unsafe {
        libc::openlog(c"reclogind".as_ptr(), libc::LOG_PID, libc::LOG_DAEMON);
        libc::syslog(libc::LOG_ERR, c"%s".as_ptr(), text.as_ptr());
        libc::closelog();
    }
}

// ---------------------------------------------------------------------------
// the last resort

static ARMED: AtomicBool = AtomicBool::new(false);
static OWNER: AtomicI32 = AtomicI32::new(0);
static TTY: AtomicI32 = AtomicI32::new(-1);
static TTY0: AtomicI32 = AtomicI32::new(-1);
static KEYBOARD_MODE: AtomicI32 = AtomicI32::new(0);
static SHOW: AtomicU32 = AtomicU32::new(0);
static OWN: AtomicU32 = AtomicU32::new(0);

/// Record how to give the console back: its descriptor, `/dev/tty0`'s, the
/// keyboard mode to restore, the console to show and our own console (the
/// console to show is only switched to while ours is the one shown). Called by
/// `Console::take` before it changes anything.
pub fn arm_last_resort(tty: RawFd, tty0: RawFd, keyboard_mode: libc::c_int, show: u32, own: u32) {
    OWN.store(own, Ordering::SeqCst);
    TTY.store(tty, Ordering::SeqCst);
    TTY0.store(tty0, Ordering::SeqCst);
    KEYBOARD_MODE.store(keyboard_mode, Ordering::SeqCst);
    SHOW.store(show, Ordering::SeqCst);
    OWNER.store(unsafe { libc::getpid() }, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
}

/// The console was given back the normal way. Called by `Console::restore`.
pub fn disarm_last_resort() {
    ARMED.store(false, Ordering::SeqCst);
}

/// Give the console back from the recorded numbers, once. Safe in a signal
/// handler; does nothing in the greeter's child before its exec.
fn last_resort() {
    if OWNER.load(Ordering::SeqCst) != unsafe { libc::getpid() } {
        return;
    }
    if ARMED.swap(false, Ordering::SeqCst) {
        console::restore_raw(
            TTY.load(Ordering::SeqCst),
            TTY0.load(Ordering::SeqCst),
            KEYBOARD_MODE.load(Ordering::SeqCst),
            SHOW.load(Ordering::SeqCst),
            OWN.load(Ordering::SeqCst),
        );
    }
}

extern "C" fn on_fatal_signal(signal: libc::c_int) {
    last_resort();
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

extern "C" fn on_stray_signal(_signal: libc::c_int) {}

/// Give every signal whose default action ends the process, and that
/// nothing else handles, a handler that does nothing: SIGALRM, SIGVTALRM,
/// SIGPROF, SIGIO (SIGPOLL), SIGPWR, SIGSTKFLT and the real-time signals.
/// None of them may end reclogind (or a session worker) without the console
/// being given back. A handler, not "ignore": exec resets handlers but keeps
/// ignored signals, and the greeter, the X server and the desktop must start
/// with the defaults. SIGUSR1 and SIGUSR2 (console switching) and SIGTERM,
/// SIGINT, SIGHUP, SIGCHLD are blocked and read from a signal descriptor;
/// they get the same handler for the moment before they are blocked.
/// SIGINT also loses an inherited "ignore" (a shell starts background
/// commands with it), so that a forwarded SIGINT reaches the descriptor.
pub fn handle_stray_signals() {
    let mut signals = vec![
        libc::SIGALRM,
        libc::SIGVTALRM,
        libc::SIGPROF,
        libc::SIGIO,
        libc::SIGPWR,
        libc::SIGSTKFLT,
        libc::SIGUSR1,
        libc::SIGUSR2,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGTERM,
    ];
    signals.extend(libc::SIGRTMIN()..=libc::SIGRTMAX());
    for signal in signals {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_stray_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Install the panic hook and the fatal-signal handlers. Call once, early.
pub fn install_last_resort() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        last_resort();
    }));
    for signal in [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGABRT,
        libc::SIGQUIT,
        libc::SIGSYS,
        libc::SIGTRAP,
        libc::SIGXCPU,
        libc::SIGXFSZ,
    ] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_fatal_signal as extern "C" fn(libc::c_int) as usize;
            // On the alternate stack the standard library sets up, so that a
            // stack overflow still gets here.
            action.sa_flags = libc::SA_ONSTACK | libc::SA_RESETHAND;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// `reclogind --reset-console N`: for the init system to run after
/// reclogind has gone, however it went. If console N is still in graphics
/// mode (nobody gave it back, as after SIGKILL), put it back in text mode
/// with the kernel's default keyboard mode and show console 1. Otherwise do
/// nothing. `None` when the arguments are not this.
pub fn reset_console_mode() -> Option<ExitCode> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--reset-console") {
        return None;
    }
    let number = match args.next().and_then(|n| n.parse::<u32>().ok()) {
        Some(n) if (1..=63).contains(&n) => n,
        _ => {
            log(WHO, "--reset-console needs a console number (1 to 63)");
            return Some(ExitCode::from(2));
        }
    };
    let open = |path: &str| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(path)
    };
    let (tty, tty0) = match (open(&format!("/dev/tty{number}")), open("/dev/tty0")) {
        (Ok(tty), Ok(tty0)) => (tty, tty0),
        (Err(e), _) | (_, Err(e)) => {
            log(WHO, format_args!("opening the consoles: {e}"));
            return Some(ExitCode::FAILURE);
        }
    };
    match console::graphics_mode(tty.as_raw_fd()) {
        Ok(false) => return Some(ExitCode::SUCCESS),
        Ok(true) => {}
        Err(e) => {
            log(WHO, format_args!("reading console {number}'s mode: {e}"));
            return Some(ExitCode::FAILURE);
        }
    }
    // The kernel's own default keyboard mode (what it resets a console to).
    let unicode = std::fs::read_to_string("/sys/module/vt/parameters/default_utf8")
        .map(|v| v.trim() != "0")
        .unwrap_or(true);
    let keyboard_mode = if unicode {
        console::K_UNICODE
    } else {
        console::K_XLATE
    };
    console::restore_raw(
        tty.as_raw_fd(),
        tty0.as_raw_fd(),
        keyboard_mode,
        FALLBACK_CONSOLE,
        number,
    );
    log(
        WHO,
        format_args!("console {number} was left in graphics mode; gave it back"),
    );
    Some(ExitCode::SUCCESS)
}
