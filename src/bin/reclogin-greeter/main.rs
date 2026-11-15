//! reclogin-greeter: the half of reclogin with no rights.
//!
//! Started by reclogind as the account `reclogin`, it receives the display
//! and its input devices as open handles over its socket, sets the mode,
//! draws the login screen on the processor, reads input and tells reclogind
//! what the person chose and typed.
//!
//! Drawing is separate from devices: a screen ([`screens`]) is a function
//! from the brand, the state from reclogind and the [`form`] (what the
//! person typed and focused) to pixels. [`kms`] and [`input`] are the only
//! modules that touch devices.
//!
//! `--render-png` draws one frame to a file for development, with no device
//! and no socket.

mod form;
mod input;
mod kms;
mod link;
mod paint;
mod sample;
mod screens;

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{Local, Timelike};
use reclogin::brand::Brand;
use reclogin::log;
use reclogin::protocol::{
    MessageKind, PromptKind, Secret, State, ToDaemon, ToGreeter, Who, VERSION,
};
use tiny_skia::Pixmap;

use form::{Action, Focus, Form};
use input::{Happening, Input};
use kms::Screen;
use link::Link;
use paint::{Area, Look};
use screens::{login, Clock, Target};

const WHO: &str = "reclogin-greeter";

const USAGE: &str = "usage: reclogin-greeter
       reclogin-greeter --render-png FILE --brand-dir DIR [--size WxH] [--state FILE.json]
                        [--at 'YYYY-MM-DD HH:MM'] [--typed N]

Without arguments it is started by reclogind, which passes its socket.
--render-png draws one frame of the login screen to FILE and exits; it opens
no device and no socket. --size defaults to 1920x1080, --state to a built-in
sample, --at to now, --typed (characters in the password field) to 12.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.is_empty() {
        run()
    } else {
        match render_options(&args) {
            Ok(Some(options)) => sample::render(&options),
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => Err(e),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn render_options(args: &[String]) -> Result<Option<sample::Options>> {
    let (mut png, mut brand_dir, mut size, mut state, mut at, mut typed) =
        (None, None, (1920, 1080), None, None, 12);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .with_context(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--render-png" => png = Some(PathBuf::from(value()?)),
            "--brand-dir" => brand_dir = Some(PathBuf::from(value()?)),
            "--size" => size = sample::parse_size(&value()?)?,
            "--state" => state = Some(PathBuf::from(value()?)),
            "--at" => at = Some(value()?),
            "--typed" => {
                typed = value()?
                    .parse::<usize>()
                    .context("--typed needs a number")?
                    .min(64)
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("reclogin-greeter {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let png = png.with_context(|| format!("--render-png is missing\n{USAGE}"))?;
    let brand_dir = brand_dir.context("--render-png needs --brand-dir")?;
    Ok(Some(sample::Options {
        png,
        brand_dir,
        size,
        state,
        at,
        typed,
    }))
}

/// The first three messages: who we talk to, the display, the state.
fn start(link: &mut Link) -> Result<(PathBuf, OwnedFd, State)> {
    let brand_dir = match link.next()? {
        (ToGreeter::Hello { version, brand_dir }, _) if version == VERSION => brand_dir,
        (ToGreeter::Hello { version, .. }, _) => {
            bail!("reclogind speaks version {version}, the greeter {VERSION}")
        }
        (other, _) => bail!(
            "expected hello, got {}",
            reclogin::protocol::Framed::kind(&other)
        ),
    };
    let display = match link.next()? {
        (ToGreeter::Display { path }, mut handles) => {
            log(WHO, format_args!("display {path}"));
            handles
                .pop()
                .context("the display came without its handle")?
        }
        (other, _) => bail!(
            "expected display, got {}",
            reclogin::protocol::Framed::kind(&other)
        ),
    };
    let state = match link.next()? {
        (ToGreeter::State(state), _) => state,
        (other, _) => bail!(
            "expected state, got {}",
            reclogin::protocol::Framed::kind(&other)
        ),
    };
    Ok((brand_dir, display, state))
}

struct Greeter {
    link: Rc<RefCell<Link>>,
    look: Look,
    screen: Screen,
    input: Input,
    state: State,
    form: Form,
    frame: Pixmap,
    hits: Vec<(Area, Target)>,
    /// The typed password, kept from Enter until PAM asks for it.
    pending: Option<Secret>,
    /// A question from PAM the next Enter answers.
    prompt: Option<(u64, PromptKind)>,
    paused: bool,
    dirty: bool,
    /// Pointer drawn into the frame because there is no hardware cursor.
    soft_pointer: Option<(f32, f32)>,
    minute: u32,
}

enum Next {
    Continue,
    Quit,
}

fn run() -> Result<()> {
    let link = Rc::new(RefCell::new(Link::from_env()?));
    let (brand_dir, display, state) = start(&mut link.borrow_mut())?;
    let brand = Brand::load(&brand_dir)?;
    let look = Look::load(brand)?;
    let screen = Screen::new(display)?;
    let (w, h) = screen.size();
    let layout = state
        .layouts
        .iter()
        .find(|l| l.name == state.current_layout)
        .or(state.layouts.first());
    let input = Input::new(link.clone(), layout, (w, h))?;
    let form = Form::new(&state);
    let frame = Pixmap::new(w, h).context("frame size")?;
    let mut greeter = Greeter {
        link,
        look,
        screen,
        input,
        state,
        form,
        frame,
        hits: Vec::new(),
        pending: None,
        prompt: None,
        paused: false,
        dirty: true,
        soft_pointer: None,
        minute: Local::now().minute(),
    };
    greeter.draw()?;
    greeter.link.borrow().send(&ToDaemon::Ready)?;
    greeter.serve()
}

impl Greeter {
    fn serve(&mut self) -> Result<()> {
        loop {
            // Messages kept while libinput waited for a device come first.
            loop {
                let kept = self.link.borrow_mut().kept();
                let Some((message, handles)) = kept else {
                    break;
                };
                if let Next::Quit = self.on_message(message, handles) {
                    return Ok(());
                }
            }
            if self.dirty && !self.paused && self.screen.ready() {
                self.draw()?;
            }

            let now = Instant::now();
            let mut due = next_minute();
            if let Some(repeat) = self.input.repeat_due() {
                due = due.min(repeat);
            }
            let timeout = due.saturating_duration_since(now).as_millis().min(60_000) as i32;
            let link_fd = self.link.borrow().fd();
            let mut fds = vec![
                libc::pollfd {
                    fd: link_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.screen.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if !self.paused {
                fds.push(libc::pollfd {
                    fd: self.input.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let n =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout.max(0)) };
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("waiting");
            }
            if fds[0].revents != 0 {
                let (message, handles) = self.link.borrow_mut().next()?;
                if let Next::Quit = self.on_message(message, handles) {
                    return Ok(());
                }
            }
            if fds[1].revents != 0 {
                self.screen.on_readable();
            }
            if fds.len() > 2 && fds[2].revents != 0 {
                for happening in self.input.read() {
                    self.on_happening(happening);
                }
            }
            if let Some(happening) = self.input.repeat_now(Instant::now()) {
                self.on_happening(happening);
            }
            let minute = Local::now().minute();
            if minute != self.minute {
                self.minute = minute;
                self.dirty = true;
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        let clock = Clock::at(Local::now(), self.state.clock_24h);
        let mut painter = self.look.begin(&mut self.frame);
        login::draw(
            &mut painter,
            &login::View {
                state: &self.state,
                form: &self.form,
                clock: &clock,
            },
        );
        self.hits = painter.into_hits();
        if let Some((x, y)) = self.soft_pointer {
            kms::draw_arrow(&mut self.frame, x, y);
        }
        self.screen.present(&self.frame)?;
        self.dirty = false;
        Ok(())
    }

    fn send(&self, message: &ToDaemon) {
        if let Err(e) = self.link.borrow().send(message) {
            log(
                WHO,
                format_args!("sending {}: {e}", reclogin::protocol::Framed::kind(message)),
            );
        }
    }

    fn on_message(&mut self, message: ToGreeter, _handles: Vec<OwnedFd>) -> Next {
        match message {
            ToGreeter::State(state) => {
                let layout_changed = state.current_layout != self.state.current_layout
                    || state.layouts != self.state.layouts;
                self.state = state;
                self.form.adopt(&self.state);
                if layout_changed {
                    if let Some(layout) = self
                        .state
                        .layouts
                        .iter()
                        .find(|l| l.name == self.state.current_layout)
                    {
                        self.input.set_layout(layout);
                    }
                }
            }
            ToGreeter::Prompt { id, kind, text } => match (kind, self.pending.take()) {
                (PromptKind::Secret, Some(secret)) => {
                    self.send(&ToDaemon::Answer {
                        prompt_id: id,
                        text: secret,
                    });
                }
                _ => {
                    // A further question (stage 2 shows these in full): the
                    // next Enter answers it with what is typed.
                    self.prompt = Some((id, kind));
                    self.form.busy = false;
                    self.form.notice = Some((MessageKind::Info, text));
                }
            },
            ToGreeter::Message { kind, text } => match &mut self.form.notice {
                Some((old_kind, old)) if self.form.busy => {
                    old.push('\n');
                    old.push_str(&text);
                    if kind == MessageKind::Error {
                        *old_kind = kind;
                    }
                }
                notice => *notice = Some((kind, text)),
            },
            ToGreeter::AuthResult { ok, text } => {
                self.form.busy = false;
                self.pending = None;
                self.prompt = None;
                if !ok {
                    self.form.clear_password();
                    self.form.focus = Focus::Password;
                    if !text.is_empty() {
                        self.form.notice = Some((MessageKind::Error, text));
                    }
                }
            }
            ToGreeter::Pause => {
                self.paused = true;
                self.input.suspend();
                self.screen.pause();
                return Next::Continue;
            }
            ToGreeter::Resume => {
                self.paused = false;
                self.input.resume();
            }
            ToGreeter::Quit => return Next::Quit,
            other => {
                log(
                    WHO,
                    format_args!(
                        "unexpected {} ignored",
                        reclogin::protocol::Framed::kind(&other)
                    ),
                );
                return Next::Continue;
            }
        }
        self.dirty = true;
        Next::Continue
    }

    fn on_happening(&mut self, happening: Happening) {
        match happening {
            Happening::Key(key) => {
                if let Some(action) = self.form.key(key, &self.state) {
                    self.act(action);
                }
            }
            Happening::CapsLock(on) => self.form.caps_lock = on,
            Happening::Pointer(x, y) => {
                if self.screen.move_pointer(x, y) {
                    self.soft_pointer = None;
                    return;
                }
                self.soft_pointer = Some((x, y));
            }
            Happening::Press(x, y) => {
                let target = self
                    .hits
                    .iter()
                    .find(|(area, _)| area.contains(x, y))
                    .map(|(_, t)| *t);
                if let Some(target) = target {
                    self.press(target);
                }
            }
        }
        self.dirty = true;
    }

    fn press(&mut self, target: Target) {
        match target {
            Target::User(i) => {
                self.form.choose(i, &self.state);
                self.form.focus = Focus::Password;
            }
            Target::Password => self.form.focus = Focus::Password,
            Target::ShowPassword => self.form.show_password = !self.form.show_password,
            Target::LogIn => {
                self.form.focus = Focus::LogIn;
                if let Some(action) = self.form.key(form::Key::Enter, &self.state) {
                    self.act(action);
                }
            }
            // The session menu and the layout switcher are later stages.
            Target::Session => self.form.focus = Focus::Session,
            Target::Layout => self.form.focus = Focus::Layout,
            // Guest, other user, the on-screen keyboard and power are later stages.
            Target::Guest | Target::OtherUser | Target::OnScreenKeyboard | Target::Power(_) => {}
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::SwitchConsole(number) => self.send(&ToDaemon::SwitchConsole { number }),
            Action::Submit => {
                let secret = Secret::copy_from(&self.form.password);
                self.form.clear_password();
                self.form.notice = None;
                self.form.busy = true;
                if let Some((prompt_id, _)) = self.prompt.take() {
                    self.send(&ToDaemon::Answer {
                        prompt_id,
                        text: secret,
                    });
                    return;
                }
                let Some(user) = self.form.chosen.and_then(|i| self.state.users.get(i)) else {
                    self.form.busy = false;
                    return;
                };
                if let Some(id) = &self.form.session {
                    self.send(&ToDaemon::ChooseSession { id: id.clone() });
                }
                self.send(&ToDaemon::Begin {
                    who: Who::User {
                        name: user.name.clone(),
                    },
                });
                self.pending = Some(secret);
            }
        }
    }
}

/// A little after the clock's next minute.
fn next_minute() -> Instant {
    let now = Local::now();
    let into = Duration::from_secs(u64::from(now.second()))
        + Duration::from_nanos(u64::from(now.nanosecond() % 1_000_000_000));
    Instant::now() + Duration::from_secs(60).saturating_sub(into) + Duration::from_millis(20)
}
