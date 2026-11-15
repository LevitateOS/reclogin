//! reclogind: the root half of reclogin.
//!
//! It owns the login console and the display, opens input devices for the
//! greeter, and starts the greeter with no rights. Logging in is `login`
//! (one login at a time), done by a session worker per login (`worker`,
//! `reclogind --session-worker`) that owns the PAM handle (`pam`) and the
//! session (`session`). Later stages add, each in its own module: `xserver`
//! (X sessions), `logind` (who is logged in, power, bringing a session to
//! the front, through the helper `reclogind --login-service`, `loginhelper`),
//! `guest`, `autologin`.

mod autologin;
mod console;
mod display;
mod greeter;
mod guest;
mod host;
mod inputs;
mod keyboard;
mod last;
mod login;
mod logind;
mod loginhelper;
mod nosetid;
mod pam;
mod sandbox;
mod session;
mod sessions;
mod state;
mod supervise;
mod users;
mod worker;
mod xserver;

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reclogin::config::{self, Config};
use reclogin::protocol::{ToDaemon, ToGreeter};
use reclogin::{brand, log};

use console::Console;
use display::Display;
use greeter::Greeter;
use inputs::Inputs;

const WHO: &str = "reclogind";

struct Options {
    console: u32,
    greeter: PathBuf,
    greeter_user: String,
    config: PathBuf,
    brand_dir: PathBuf,
    /// The greeter's lock-down (`sandbox.rs`); off with `--no-sandbox`.
    sandbox: bool,
}

const USAGE: &str = "usage: reclogind [--console N] [--greeter PATH] [--greeter-user NAME]
                 [--config FILE] [--brand-dir DIR] [--no-sandbox]
       reclogind --reset-console N

Runs the login screen on console N (default 7). Must run as root.
  --greeter PATH       the greeter program (default: reclogin-greeter next to reclogind)
  --greeter-user NAME  the account the greeter runs as (default: reclogin)
  --config FILE        settings (default: /etc/reclogin.conf)
  --brand-dir DIR      brand data for the greeter (default: /usr/share/reclogin)
  --no-sandbox         development: start the greeter without its lock-down
                       (network namespace, limits, capabilities, Landlock)
  --reset-console N    for the init system after reclogind has gone: if console N
                       is still in graphics mode, give it back and show console 1

Exit status: 0 stopped on request, 1 an error, 2 bad arguments, 3 gave up
after the greeter failed three times within a minute.";

fn options() -> Result<Option<Options>> {
    let own = std::env::current_exe().context("finding reclogind's own path")?;
    let mut options = Options {
        console: 7,
        greeter: own.with_file_name("reclogin-greeter"),
        greeter_user: "reclogin".into(),
        config: PathBuf::from(config::DEFAULT_PATH),
        brand_dir: PathBuf::from(brand::DEFAULT_DIR),
        sandbox: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--console" => {
                options.console = value()?.parse().context("--console needs a number")?
            }
            "--greeter" => options.greeter = value()?.into(),
            "--greeter-user" => options.greeter_user = value()?,
            "--config" => options.config = value()?.into(),
            "--brand-dir" => options.brand_dir = value()?.into(),
            "--no-sandbox" => options.sandbox = false,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("reclogind {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(Some(options))
}

fn main() -> ExitCode {
    // Started by reclogind itself for one login (`login.rs`); not in USAGE.
    if std::env::args().nth(1).as_deref() == Some(worker::SWITCH) {
        return worker::run();
    }
    // Started by reclogind itself for the login service (`logind.rs`); not
    // in USAGE.
    if std::env::args().nth(1).as_deref() == Some(logind::SWITCH) {
        return loginhelper::run();
    }
    // Started by the guest's cleanup (`guest.rs`) for one call; not in USAGE.
    if std::env::args().nth(1).as_deref() == Some(loginhelper::END_USER) {
        return loginhelper::end_user();
    }
    // The guest's cleanup, started by reclogind with the guest lock
    // (`guest.rs`); not in USAGE.
    if std::env::args().nth(1).as_deref() == Some(guest::CLEANUP_SWITCH) {
        return guest::cleanup_child();
    }
    if let Some(code) = supervise::reset_console_mode() {
        return code;
    }
    let options = match options() {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("{e:#}"));
            return ExitCode::from(2);
        }
    };
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// The signals handled through a signal descriptor instead of handlers.
fn signal_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [
            console::RELEASE_SIGNAL,
            console::ACQUIRE_SIGNAL,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGCHLD,
        ] {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

fn signal_fd() -> io::Result<OwnedFd> {
    let set = signal_set();
    unsafe {
        if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn next_signal(fd: &OwnedFd) -> Option<i32> {
    let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    let got = unsafe {
        libc::read(
            fd.as_raw_fd(),
            (&mut info as *mut libc::signalfd_siginfo).cast(),
            size,
        )
    };
    (got == size as isize).then_some(info.ssi_signo as i32)
}

struct Daemon {
    console: Console,
    display: Display,
    inputs: Inputs,
    greeter: Greeter,
    /// Whether the login console is shown (and the display and inputs ours).
    shown: bool,
    /// Restarting the greeter, and giving up (`supervise.rs`).
    supervisor: supervise::Supervisor,
    /// The login under way and its session (`login.rs`).
    login: login::Login,
    /// Who is logged in, power, returning to a session (`logind.rs`).
    logind: logind::Service,
}

enum Next {
    Continue,
    Stop,
}

fn run(options: Options) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("reclogind must run as root");
    }
    // Before the console is touched: a panic or a fatal signal still gives
    // it back.
    supervise::handle_stray_signals();
    supervise::install_last_resort();
    // Block the signals first, so that a console switch during start-up waits
    // in the descriptor instead of killing us.
    let signals = signal_fd().context("setting up signals")?;
    let loaded = Config::load(&options.config);
    for problem in &loaded.problems {
        log(WHO, problem);
    }
    // The guest's setting and account (`guest.rs`); its cleanup starts
    // below, and the screen offers Guest only once it finished clean.
    let guest = guest::at_start(&loaded.config);

    // The login service first (its helper has a moment to report): a console
    // that belongs to a running user session is not taken from it.
    let logind = logind::Service::start();
    let active = console::active_console();
    let take_over = !active.is_some_and(|n| logind.session_on(n));
    // The display before the console: at boot its driver may still be
    // loading, and the console shown stays usable while we wait for it.
    let display = Display::open_waiting()?;
    // Every early return below drops `console`, which gives it back.
    let mut console = Console::take(options.console, take_over)?;
    if take_over {
        display
            .take()
            .with_context(|| format!("becoming the master of {}", display.path))?;
    } else {
        // Opening it may have made us its master (a text console in
        // front): a desktop started from there must be able to have it.
        display.give_up_quietly();
        log(
            WHO,
            "a user session's console is shown; the login screen waits until its console is shown",
        );
    }
    log(WHO, format_args!("display {}", display.path));
    greeter::configure(greeter::Settings {
        brand_dir: options.brand_dir.clone(),
        sandbox: options.sandbox,
    });
    let mut supervisor = supervise::Supervisor::new(
        options.greeter.clone(),
        options.greeter_user.clone(),
        options.brand_dir.clone(),
        guest.config_for_screen(&loaded.config),
    );
    supervisor.view = logind.view.clone();
    let mut inputs = Inputs::default();
    // Started, sent hello, the display and the state; a greeter that cannot
    // be started counts as a failure like one that crashes.
    let greeter = match supervisor.launch_until_up(&display, take_over) {
        Ok(greeter) => greeter,
        Err(reason) => supervise::give_up(
            &mut console,
            &display,
            &mut inputs,
            &reason,
            supervisor.missing(),
        ),
    };
    let mut daemon = Daemon {
        console,
        display,
        inputs,
        greeter,
        shown: take_over,
        supervisor,
        login: login::Login::default(),
        logind,
    };
    daemon.login.guest = guest;
    // The guest's cleanup at start, in a child process (`guest.rs`), before
    // the automatic login (which, for the guest, waits for it).
    daemon.start_guest_cleanup("at start");
    // Automatic login, once, now that the greeter is up to show a failure
    // (`autologin.rs`).
    daemon.start_autologin(&loaded.config, take_over);
    if !take_over {
        // The session in front: when it ends and nothing else is shown, the
        // login console is (`logind.rs`).
        daemon.logind.foreground = active;
    }

    let result = daemon.serve(&signals);
    daemon.end_login();
    daemon.stop_greeter();
    daemon.logind.stop();
    daemon.give_back();
    result
}

impl Daemon {
    fn serve(&mut self, signals: &OwnedFd) -> Result<()> {
        loop {
            self.login_check_greeter();
            if let Next::Stop = self.tick() {
                return Ok(());
            }
            let greeter_watched = !self.login.greeter_away();
            let mut fds = [
                libc::pollfd {
                    fd: signals.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    // While a session runs no greeter does: its closed socket
                    // is not watched (`login.rs`).
                    fd: if greeter_watched {
                        self.greeter.channel.as_raw_fd()
                    } else {
                        -1
                    },
                    events: if self.greeter.wants_write() {
                        libc::POLLIN | libc::POLLOUT
                    } else {
                        libc::POLLIN
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.login.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.logind.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = self.poll_timeout();
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("waiting");
            }
            let generation = self.supervisor.generation;
            if fds[0].revents != 0 {
                let mut console_signal = false;
                while let Some(signal) = next_signal(signals) {
                    console_signal |=
                        signal == console::RELEASE_SIGNAL || signal == console::ACQUIRE_SIGNAL;
                    if let Next::Stop = self.on_signal(signal) {
                        return Ok(());
                    }
                }
                // Which console is shown decides, not the signals' order
                // (`console.rs`).
                if console_signal {
                    if let Next::Stop = self.console_changed() {
                        return Ok(());
                    }
                }
            }
            // A new greeter was started: `fds[1]` was about the old socket.
            if self.supervisor.generation != generation {
                continue;
            }
            if fds[1].revents & libc::POLLOUT != 0 {
                if let Err(e) = self.greeter.flush() {
                    log(WHO, format_args!("sending to the greeter: {e}"));
                    if let Next::Stop = self.greeter_gone("could not be reached") {
                        return Ok(());
                    }
                    continue;
                }
            }
            if fds[1].revents & !libc::POLLOUT != 0 {
                let gone = if self.login.expects_raw_answer() {
                    // The secret after `answer`, raw (`login.rs`).
                    self.on_raw_answer()
                } else {
                    match self.greeter.channel.recv::<ToDaemon>() {
                        Ok(Some((message, _))) => self.on_message(message),
                        Ok(None) => self.greeter_gone("closed its socket"),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Next::Continue,
                        Err(e) => {
                            log(WHO, format_args!("from the greeter: {e}"));
                            if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                                self.greeter_gone("hung up")
                            } else {
                                Next::Continue
                            }
                        }
                    }
                };
                if let Next::Stop = gone {
                    return Ok(());
                }
            }
            // The session worker of the login under way (`login.rs`).
            if fds[2].revents != 0 {
                if let Next::Stop = self.on_worker() {
                    return Ok(());
                }
            }
            // The login-service helper (`logind.rs`).
            if fds[3].revents != 0 {
                if let Next::Stop = self.on_logind() {
                    return Ok(());
                }
            }
        }
    }

    /// Send `message`; a greeter that cannot be reached is replaced.
    /// Never waits: a message the socket cannot take now is queued
    /// (`Greeter::send`), and a greeter that stops reading is hung (`tick`).
    fn send(&mut self, message: &ToGreeter) -> Next {
        // A greeter still waiting for its display and state gets nothing
        // else: what is sent meanwhile belonged to the greeter it replaced
        // (an `auth_result`, a power answer) or is renewed by the handshake
        // (the state, pause and resume). The line kept for the next screen
        // waits in `login.rs` and is sent after the handshake.
        if self.supervisor.handshake_pending {
            log(
                WHO,
                format_args!(
                    "{} dropped: the greeter is still starting",
                    reclogin::protocol::Framed::kind(message)
                ),
            );
            return Next::Continue;
        }
        // Text too long for one packet (PAM's, the service's) is cut.
        let fitted = greeter::fit_text(message);
        let message = fitted.as_ref().unwrap_or(message);
        match self.greeter.send(message, &[]) {
            Ok(()) => Next::Continue,
            // Could not be encoded: reclogind's own problem, never the
            // greeter's, and not counted against it.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                log(
                    WHO,
                    format_args!(
                        "{} not sent (reclogind's fault, not the greeter's): {e}",
                        reclogin::protocol::Framed::kind(message)
                    ),
                );
                Next::Continue
            }
            Err(e) => {
                log(
                    WHO,
                    format_args!(
                        "sending {} to the greeter: {e}",
                        reclogin::protocol::Framed::kind(message)
                    ),
                );
                self.greeter_gone("could not be reached")
            }
        }
    }

    /// The timed checks, on every turn of the loop: a hung greeter (its
    /// queue stuck, a ping unanswered) is replaced and counted; a ping is
    /// sent when due while the login console is shown; then the login's own
    /// (`login.rs`).
    fn tick(&mut self) -> Next {
        if let Next::Stop = self.logind_tick() {
            return Next::Stop;
        }
        let now = Instant::now();
        if !self.login.greeter_away() {
            if let Some(why) = self.greeter.hung(now) {
                return self.greeter_gone(why);
            }
            if self.shown && !self.supervisor.handshake_pending {
                if let Some(seq) = self.greeter.ping_due(now) {
                    if let Next::Stop = self.send(&ToGreeter::Ping { seq }) {
                        return Next::Stop;
                    }
                }
            }
        }
        self.login_tick()
    }

    /// How long `poll` may wait: until the greeter's next check, or a
    /// quarter second while the login has something timed; forever when
    /// nothing is.
    fn poll_timeout(&self) -> libc::c_int {
        // Something timed in the login service's helper (a restart, a return
        // to check) shortens the wait.
        // A running session's desktop is looked at now and then
        // (`login.rs`).
        let other = match (self.logind.wait_ms(), self.login.wait_ms()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (self.greeter_timeout(), other) {
            (-1, Some(ms)) => ms,
            (base, Some(ms)) => base.min(ms),
            (base, None) => base,
        }
    }

    fn greeter_timeout(&self) -> libc::c_int {
        if self.login.wants_tick() {
            return 250;
        }
        if self.login.greeter_away() {
            return -1;
        }
        let pinging = self.shown && !self.supervisor.handshake_pending;
        match self.greeter.next_deadline(pinging) {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                (left.as_millis().min(60_000) as libc::c_int).max(10)
            }
            None => -1,
        }
    }

    fn on_signal(&mut self, signal: i32) -> Next {
        match signal {
            // Each is answered as the kernel requires; what follows is
            // decided by which console is shown (`Daemon::console_changed`).
            console::RELEASE_SIGNAL => {
                if self.shown {
                    self.release_display();
                }
                if let Err(e) = self.console.allow_release() {
                    log(WHO, format_args!("letting the console go: {e}"));
                }
                Next::Continue
            }
            console::ACQUIRE_SIGNAL => {
                if let Err(e) = self.console.acknowledge_acquire() {
                    log(WHO, format_args!("taking the console back: {e}"));
                }
                Next::Continue
            }
            libc::SIGCHLD => {
                // The session worker (`login.rs`), then the greeter.
                if let Next::Stop = self.login_reap() {
                    return Next::Stop;
                }
                match self.greeter.child.try_wait() {
                    Ok(Some(_)) => self.greeter_gone("ended"),
                    _ => Next::Continue,
                }
            }
            other => {
                log(WHO, format_args!("stopping on signal {other}"));
                Next::Stop
            }
        }
    }

    fn on_message(&mut self, message: ToDaemon) -> Next {
        // While the login console is not shown the greeter is obeyed in
        // nothing that acts (`greeter.rs`, `Daemon::refuse_while_paused`).
        let message = if self.shown && self.console.is_shown() {
            message
        } else {
            match self.refuse_while_paused(message) {
                Ok(message) => message,
                Err(next) => return next,
            }
        };
        match message {
            ToDaemon::Ready => {
                log(WHO, "the greeter is ready");
                self.greeter.mark_ready();
                Next::Continue
            }
            ToDaemon::OpenDevice { request_id, path } => {
                // Only to a greeter on the login console, asked of the
                // kernel each time.
                let opened = if self.shown && self.console.is_shown() {
                    self.inputs
                        .open(self.supervisor.generation, request_id, &path)
                } else {
                    Err("the login console is not shown".to_string())
                };
                match opened {
                    Ok(fd) => match self
                        .greeter
                        .send(&ToGreeter::Device { request_id }, &[fd.as_fd()])
                    {
                        Ok(()) => Next::Continue,
                        Err(e) => {
                            log(WHO, format_args!("sending a device to the greeter: {e}"));
                            self.greeter_gone("could not be reached")
                        }
                    },
                    Err(reason) => {
                        log(
                            WHO,
                            format_args!("not opening {}: {reason}", path.display()),
                        );
                        self.send(&ToGreeter::DeviceRefused { request_id, reason })
                    }
                }
            }
            ToDaemon::CloseDevice { request_id } => {
                self.inputs.close(request_id);
                Next::Continue
            }
            message @ (ToDaemon::Begin { .. }
            | ToDaemon::Answer { .. }
            | ToDaemon::Cancel
            | ToDaemon::ChooseSession { .. }) => self.on_login_message(message),
            ToDaemon::Pong { seq } => {
                self.greeter.pong(seq);
                Next::Continue
            }
            ToDaemon::SwitchConsole { number } => {
                // Ctrl+Alt+F1 to F12 only. A login in progress is not lost:
                // PAM's conversation goes on in reclogind, and the greeter
                // clears a half-typed field on `pause` and says so.
                if (1..=12).contains(&number) && number != self.console.number {
                    if let Err(e) = self.console.switch_to(number) {
                        log(WHO, format_args!("switching to console {number}: {e}"));
                    }
                }
                Next::Continue
            }
            // The layout picked on the screen lasts until the greeter is
            // restarted; nothing is kept here (`keyboard.rs`).
            ToDaemon::SetLayout { .. } => Next::Continue,
            ToDaemon::ReturnTo { user } => self.on_return_to(user),
            ToDaemon::EndGuest => self.on_end_guest(),
            ToDaemon::Power { action, confirmed } => self.on_power(action, confirmed),
        }
    }

    /// Ask the greeter to quit, then make sure it has. Its exit is expected:
    /// it is not counted as a failure and no new greeter is started.
    fn stop_greeter(&mut self) {
        self.supervisor.stopping(self.greeter.child.id());
        if let Ok(Some(_)) = self.greeter.child.try_wait() {
            return;
        }
        let _ = self.greeter.send(&ToGreeter::Quit, &[]);
        let _ = self.greeter.flush();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.greeter.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log(WHO, "the greeter did not quit; stopping it");
        let _ = self.greeter.child.kill();
        let _ = self.greeter.child.wait();
    }

    /// The inputs revoked, the display given up, the console as it was.
    fn give_back(&mut self) {
        self.inputs.revoke_all();
        self.display.give_up();
        self.console.restore();
    }
}
