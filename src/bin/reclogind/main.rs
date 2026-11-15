//! reclogind: the root half of reclogin.
//!
//! It owns the login console and the display, opens input devices for the
//! greeter, and starts the greeter with no rights. Later stages add, each in
//! its own module: `pam` (the PAM conversation), `session` (starting and
//! ending a desktop), `xserver` (X sessions), `logind` (who is logged in,
//! power, bringing a session to the front), `guest`, `autologin`, and
//! restarting a crashed greeter.
//!
//! Stage 1 answers a login attempt with a message that logging in is not
//! built yet.

mod console;
mod display;
mod greeter;
mod host;
mod inputs;
mod keyboard;
mod sessions;
mod state;
mod users;

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reclogin::config::{self, Config};
use reclogin::protocol::{MessageKind, ToDaemon, ToGreeter, VERSION};
use reclogin::{brand, log};

use console::Console;
use display::Display;
use greeter::Greeter;
use inputs::Inputs;

const WHO: &str = "reclogind";
const NOT_BUILT: &str = "Logging in is not built yet.";

struct Options {
    console: u32,
    greeter: PathBuf,
    greeter_user: String,
    config: PathBuf,
    brand_dir: PathBuf,
}

const USAGE: &str = "usage: reclogind [--console N] [--greeter PATH] [--greeter-user NAME]
                 [--config FILE] [--brand-dir DIR]

Runs the login screen on console N (default 7). Must run as root.
  --greeter PATH       the greeter program (default: reclogin-greeter next to reclogind)
  --greeter-user NAME  the account the greeter runs as (default: reclogin)
  --config FILE        settings (default: /etc/reclogin.conf)
  --brand-dir DIR      brand data for the greeter (default: /usr/share/reclogin)";

fn options() -> Result<Option<Options>> {
    let own = std::env::current_exe().context("finding reclogind's own path")?;
    let mut options = Options {
        console: 7,
        greeter: own.with_file_name("reclogin-greeter"),
        greeter_user: "reclogin".into(),
        config: PathBuf::from(config::DEFAULT_PATH),
        brand_dir: PathBuf::from(brand::DEFAULT_DIR),
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
}

enum Next {
    Continue,
    Stop,
}

fn run(options: Options) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("reclogind must run as root");
    }
    // Block the signals first, so that a console switch during start-up waits
    // in the descriptor instead of killing us.
    let signals = signal_fd().context("setting up signals")?;
    let loaded = Config::load(&options.config);
    for problem in &loaded.problems {
        log(WHO, problem);
    }

    let console = Console::take(options.console)?;
    let display = Display::open()?;
    display
        .take()
        .with_context(|| format!("becoming the master of {}", display.path))?;
    log(WHO, format_args!("display {}", display.path));
    let greeter = greeter::start(&options.greeter, &options.greeter_user)?;
    let mut daemon = Daemon {
        console,
        display,
        inputs: Inputs::default(),
        greeter,
        shown: true,
    };

    let state = state::read(&loaded.config);
    log(
        WHO,
        format_args!(
            "{} users, {} sessions, layout {}",
            state.users.len(),
            state.sessions.len(),
            state.current_layout
        ),
    );
    let started = (|| -> io::Result<()> {
        let channel = &daemon.greeter.channel;
        channel.send(
            &ToGreeter::Hello {
                version: VERSION,
                brand_dir: options.brand_dir.clone(),
            },
            &[],
        )?;
        channel.send(
            &ToGreeter::Display {
                path: daemon.display.path.clone(),
            },
            &[daemon.display.as_fd()],
        )?;
        channel.send(&ToGreeter::State(state), &[])
    })();
    if let Err(e) = started {
        log(WHO, format_args!("talking to the greeter: {e}"));
        daemon.stop_greeter();
        daemon.give_back();
        return Err(e).context("starting the greeter");
    }

    let result = daemon.serve(&signals);
    daemon.stop_greeter();
    daemon.give_back();
    result
}

impl Daemon {
    fn serve(&mut self, signals: &OwnedFd) -> Result<()> {
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: signals.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.greeter.channel.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("waiting");
            }
            if fds[0].revents != 0 {
                while let Some(signal) = next_signal(signals) {
                    if let Next::Stop = self.on_signal(signal) {
                        return Ok(());
                    }
                }
            }
            if fds[1].revents != 0 {
                match self.greeter.channel.recv::<ToDaemon>() {
                    Ok(Some((message, _))) => {
                        if let Next::Stop = self.on_message(message) {
                            return Ok(());
                        }
                    }
                    Ok(None) => {
                        log(WHO, "the greeter closed its socket");
                        return Ok(());
                    }
                    Err(e) => {
                        log(WHO, format_args!("from the greeter: {e}"));
                        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn send(&self, message: &ToGreeter) -> Next {
        match self.greeter.channel.send(message, &[]) {
            Ok(()) => Next::Continue,
            Err(e) => {
                log(
                    WHO,
                    format_args!(
                        "sending {} to the greeter: {e}",
                        reclogin::protocol::Framed::kind(message)
                    ),
                );
                Next::Stop
            }
        }
    }

    fn on_signal(&mut self, signal: i32) -> Next {
        match signal {
            console::RELEASE_SIGNAL => {
                if self.shown {
                    self.shown = false;
                    self.inputs.revoke_all();
                    self.display.give_up();
                    let _ = self.send(&ToGreeter::Pause);
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
                if !self.shown {
                    self.shown = true;
                    if let Err(e) = self.display.take() {
                        log(WHO, format_args!("taking the display back: {e}"));
                    }
                    return self.send(&ToGreeter::Resume);
                }
                Next::Continue
            }
            libc::SIGCHLD => match self.greeter.child.try_wait() {
                Ok(Some(status)) => {
                    log(WHO, format_args!("the greeter ended: {status}"));
                    Next::Stop
                }
                _ => Next::Continue,
            },
            other => {
                log(WHO, format_args!("stopping on signal {other}"));
                Next::Stop
            }
        }
    }

    fn on_message(&mut self, message: ToDaemon) -> Next {
        match message {
            ToDaemon::Ready => {
                log(WHO, "the greeter is ready");
                Next::Continue
            }
            ToDaemon::OpenDevice { request_id, path } => {
                let opened = if self.shown {
                    self.inputs.open(request_id, &path)
                } else {
                    Err("the login console is not shown".to_string())
                };
                match opened {
                    Ok(fd) => match self
                        .greeter
                        .channel
                        .send(&ToGreeter::Device { request_id }, &[fd.as_fd()])
                    {
                        Ok(()) => Next::Continue,
                        Err(e) => {
                            log(WHO, format_args!("sending a device to the greeter: {e}"));
                            Next::Stop
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
            ToDaemon::Begin { .. } | ToDaemon::Answer { .. } => {
                // An Answer's secret is overwritten as `message` is dropped here.
                if let Next::Stop = self.send(&ToGreeter::Message {
                    kind: MessageKind::Error,
                    text: NOT_BUILT.into(),
                }) {
                    return Next::Stop;
                }
                self.send(&ToGreeter::AuthResult {
                    ok: false,
                    text: String::new(),
                })
            }
            ToDaemon::SwitchConsole { number } => {
                if (1..=63).contains(&number) && number != self.console.number {
                    if let Err(e) = self.console.switch_to(number) {
                        log(WHO, format_args!("switching to console {number}: {e}"));
                    }
                }
                Next::Continue
            }
            other @ (ToDaemon::Cancel
            | ToDaemon::ChooseSession { .. }
            | ToDaemon::ReturnTo { .. }
            | ToDaemon::Power { .. }
            | ToDaemon::SetLayout { .. }) => {
                log(
                    WHO,
                    format_args!(
                        "{} is not built yet; ignored",
                        reclogin::protocol::Framed::kind(&other)
                    ),
                );
                Next::Continue
            }
        }
    }

    /// Ask the greeter to quit, then make sure it has.
    fn stop_greeter(&mut self) {
        if let Ok(Some(_)) = self.greeter.child.try_wait() {
            return;
        }
        let _ = self.greeter.channel.send(&ToGreeter::Quit, &[]);
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
