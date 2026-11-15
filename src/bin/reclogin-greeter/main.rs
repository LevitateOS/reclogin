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
mod osk;
mod paint;
mod sample;
mod sandbox;
mod screens;
mod watch;

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
    MessageKind, PromptKind, Secret, State, ToDaemon, ToGreeter, Who, GREETER_EXIT_NO_MODE, VERSION,
};
use tiny_skia::Pixmap;

use form::{Action, Focus, Form, Goal, Row};
use input::{Happening, Input};
use kms::Screen;
use link::Link;
use paint::{Area, Look};
use screens::{login, Clock, Target};

const WHO: &str = "reclogin-greeter";

const USAGE: &str = "usage: reclogin-greeter [--no-sandbox]
       reclogin-greeter --render-png FILE --brand-dir DIR [--size WxH] [--state FILE.json]
                        [--at 'YYYY-MM-DD HH:MM'] [--typed N] [--second-screen]

Without arguments it is started by reclogind, which passes its socket and
has locked it down; --no-sandbox (passed by `reclogind --no-sandbox`) lets it
run without that lock-down. --render-png draws one frame of the login screen to FILE and exits; it opens
no device and no socket. --size defaults to 1920x1080, --state to a built-in
sample, --at to now, --typed (characters in the password field) to 12.
--second-screen draws what the other monitors show instead of the login.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.is_empty() || args == ["--no-sandbox"] {
        run(args.is_empty())
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
            // reclogind names this one in its log (and counts it).
            if e.downcast_ref::<NoMode>().is_some() {
                return ExitCode::from(GREETER_EXIT_NO_MODE);
            }
            ExitCode::FAILURE
        }
    }
}

/// The display took no mode (or showed no first frame): the exit status
/// says so (`GREETER_EXIT_NO_MODE`).
#[derive(Debug)]
struct NoMode;

impl std::fmt::Display for NoMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the display could not be set up")
    }
}

impl std::error::Error for NoMode {}

fn render_options(args: &[String]) -> Result<Option<sample::Options>> {
    let (mut png, mut brand_dir, mut size, mut state, mut at, mut typed) =
        (None, None, (1920, 1080), None, None, 12);
    let mut second = false;
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
            "--second-screen" => second = true,
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
        second,
    }))
}

/// The first three messages: who we talk to, the display, the state.
/// reclogind sends nothing else in between; anything else that comes (from
/// an older reclogind, say) is passed over rather than taken as an error,
/// and `quit` ends the greeter quietly (`None`).
fn start(link: &mut Link) -> Result<Option<(PathBuf, OwnedFd, State)>> {
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
    let passed_over = |other: &ToGreeter, waiting: &str| {
        log(
            WHO,
            format_args!(
                "{} before the {waiting} passed over",
                reclogin::protocol::Framed::kind(other)
            ),
        );
    };
    let display = loop {
        match link.next()? {
            (ToGreeter::Display { path }, mut handles) => {
                log(WHO, format_args!("display {path}"));
                break handles
                    .pop()
                    .context("the display came without its handle")?;
            }
            (ToGreeter::Quit, _) => return Ok(None),
            (other, _) => passed_over(&other, "display"),
        }
    };
    let state = loop {
        match link.next()? {
            (ToGreeter::State(state), _) => break *state,
            (ToGreeter::Quit, _) => return Ok(None),
            (other, _) => passed_over(&other, "state"),
        }
    };
    Ok(Some((brand_dir, display, state)))
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
    paused: bool,
    dirty: bool,
    /// Pointer drawn into the frame because there is no hardware cursor.
    soft_pointer: Option<(f32, f32)>,
    minute: u32,
    watch: watch::Watch,
}

enum Next {
    Continue,
    Quit,
}

/// `locked`: started by reclogind with the lock-down (no `--no-sandbox`).
fn run(locked: bool) -> Result<()> {
    sandbox::check(locked)?;
    let link = Rc::new(RefCell::new(Link::from_env()?));
    // No new sockets from here on (Unix sockets in the file system, the
    // system bus, the log sockets): before anything is read from reclogind
    // or from input. The inherited socket keeps working.
    if locked {
        sandbox::no_new_sockets()?;
    }
    let Some((brand_dir, display, state)) = start(&mut link.borrow_mut())? else {
        return Ok(());
    };
    let brand = Brand::load(&brand_dir)?;
    let look = Look::load(brand)?;
    let screen = Screen::new(display).map_err(|e| e.context(NoMode))?;
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
        paused: false,
        dirty: true,
        soft_pointer: None,
        minute: Local::now().minute(),
        watch: watch::Watch::new(),
    };
    // The first frame sets the mode; `ready` (from which reclogind counts
    // pings) follows it.
    greeter.draw().map_err(|e| e.context(NoMode))?;
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
            if self.dirty && !self.paused && !self.watch.asleep && self.screen.ready() {
                self.draw()?;
            }

            let now = Instant::now();
            let mut due = next_minute().min(self.watch_due());
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
            self.tick();
            let minute = Local::now().minute();
            if minute != self.minute {
                self.minute = minute;
                self.dirty = true;
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        self.sync_keyboard();
        let clock = Clock::at(Local::now(), self.state.clock_24h);
        let mut painter = self.look.begin(&mut self.frame);
        screens::draw(
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
        self.draw_others();
        self.dirty = false;
        self.after_draw();
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

    /// Send a secret answer (`answer`, then the raw secret); `secret` is
    /// overwritten when the caller drops it.
    fn send_answer(&self, prompt_id: u64, secret: &Secret) {
        if let Err(e) = self.link.borrow().send_answer(prompt_id, secret) {
            log(WHO, format_args!("sending answer: {e}"));
        }
    }

    fn on_message(&mut self, message: ToGreeter, _handles: Vec<OwnedFd>) -> Next {
        match message {
            ToGreeter::State(state) => {
                let layout_changed = state.current_layout != self.state.current_layout
                    || state.layouts != self.state.layouts;
                self.state = *state;
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
            // Sent before reclogind saw the cancel: the attempt is ending.
            ToGreeter::Prompt { .. } | ToGreeter::Message { .. } if self.form.cancelling => {
                return Next::Continue;
            }
            ToGreeter::Prompt { id, kind, text } => {
                // PAM's first question, when it is a secret one, takes the
                // password typed on the login screen. Any other question is
                // shown as a field of its own; a password still waiting is
                // overwritten as it is dropped here, so it can never answer
                // a later question (a new password, say).
                let first = !self.form.talk.asked;
                self.form.talk.asked = true;
                match (first, kind, self.pending.take()) {
                    (true, PromptKind::Secret, Some(secret)) => {
                        self.send_answer(id, &secret);
                    }
                    _ => self.form.ask(id, kind, text),
                }
            }
            ToGreeter::Message { kind, text } => self.form.talk.messages.push((kind, text)),
            // A power action was refused or failed: the service's reason,
            // as the error line (the question, if open, stays).
            ToGreeter::PowerRefused { text } => {
                self.form.notice = Some((MessageKind::Error, text));
            }
            // reclogind's question before a restart or shut down while
            // people are logged in: its list, not the greeter's own idea.
            ToGreeter::PowerQuestion { action, people } => {
                if self.paused {
                    return Next::Continue;
                }
                self.form.ask_power(action, people, &self.state);
            }
            ToGreeter::AuthResult { ok, text } => {
                self.pending = None;
                // The answer to a cancel (or the attempt's own end, which
                // crossed it): the screen is free again.
                let cancelled = std::mem::take(&mut self.form.cancelling);
                self.form.finish(ok, text, &self.state);
                // An answer to `return_to`: success hands the display away
                // (the screen is not shown any more), so nothing waits.
                if std::mem::take(&mut self.form.returning) {
                    self.form.busy = false;
                }
                if cancelled {
                    self.form.talk.messages.clear();
                }
            }
            ToGreeter::Ping { seq } => {
                // Liveness: reclogind takes a greeter that does not answer
                // within ten seconds as hung.
                self.send(&ToDaemon::Pong { seq });
                return Next::Continue;
            }
            ToGreeter::Pause => {
                self.paused = true;
                // A typed password does not wait on a screen nobody sees.
                // Not silently: the screen says so when it is shown again.
                // A password already sent (`pending`) is not affected.
                if !self.form.password.is_empty() {
                    self.form.clear_password();
                    self.form.notice = Some((
                        MessageKind::Info,
                        "The password was cleared when you left this screen.".into(),
                    ));
                }
                // Nor does a question about power wait on it.
                self.form.close_question(&self.state);
                self.input.suspend();
                self.screen.pause();
                self.watch.paused();
                return Next::Continue;
            }
            ToGreeter::Resume => {
                self.paused = false;
                self.watch.resumed();
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
        let Some(happening) = self.admit(happening) else {
            self.dirty = true;
            return;
        };
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
                let touched = self.take_touch_down();
                // The last one drawn is on top (the session menu).
                let target = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(area, _)| area.contains(x, y))
                    .map(|(_, t)| *t);
                if let Some(target) = target {
                    self.press(target);
                    self.after_press(target, touched);
                }
            }
            // Handled (and turned into presses) by `admit`.
            Happening::Release | Happening::Touch(..) => {}
        }
        self.dirty = true;
    }

    fn press(&mut self, target: Target) {
        // An open session menu takes the click: an entry picks, anything
        // else closes it (the label itself toggles).
        if self.form.menu.is_some() {
            match target {
                Target::SessionOption(i) => {
                    if let Some(action) = self.form.pick_session(i, &self.state) {
                        self.act(action);
                    }
                }
                Target::Menu => {}
                Target::Session => self.form.menu = None,
                _ => {
                    self.form.menu = None;
                    self.press_outside_menu(target);
                }
            }
            return;
        }
        self.press_outside_menu(target);
    }

    fn press_outside_menu(&mut self, target: Target) {
        if self.form.question.is_some() {
            self.press_question(target);
            return;
        }
        match target {
            Target::User(i) => {
                if let Some(user) = self.state.users.get(i) {
                    self.form.select(Row::User(user.name.clone()), &self.state);
                    self.form.entered(&self.state);
                }
            }
            Target::Guest => {
                self.form.select(Row::Guest, &self.state);
                self.form.entered(&self.state);
            }
            Target::OtherUser => {
                self.form.select(Row::Other, &self.state);
                self.form.entered(&self.state);
            }
            Target::UserName => self.form.focus = Focus::UserName,
            Target::Password => self.form.focus = Focus::Password,
            Target::ShowPassword => self.form.show_password = !self.form.show_password,
            Target::LogIn => {
                self.form.focus = Focus::LogIn;
                if let Some(action) = self.form.key(form::Key::Enter, &self.state) {
                    self.act(action);
                }
            }
            Target::Cancel => self.act(Action::Cancel),
            Target::Session => self.form.open_menu(&self.state),
            Target::Layout => {
                if self.state.layouts.len() > 1 {
                    self.form.focus = Focus::Layout;
                    self.act(Action::NextLayout);
                }
            }
            Target::Power(action) => {
                self.form.focus = Focus::Power(action);
                if let Some(action) = self.form.press_power(action, &self.state) {
                    self.act(action);
                }
            }
            Target::EndGuest => self.end_guest(),
            Target::SessionOption(_) | Target::Menu | Target::Confirm => {}
            Target::OnScreenKeyboard => osk::open(&mut self.form, &self.state),
            Target::Key(id) => self.press_key(id),
        }
    }

    /// "End the guest session": reclogind answers with `auth_result`, `ok`
    /// when the session was asked to end (the screen then follows the next
    /// state, which no longer has the guest running), else why not.
    fn end_guest(&mut self) {
        if self.form.busy || !form::guest_running(&self.state) {
            return;
        }
        self.form.notice = None;
        self.form.busy = true;
        self.form.returning = true;
        self.send(&ToDaemon::EndGuest);
    }

    /// A click while the power question is shown: "… anyway", "Cancel", a
    /// footer action, or the layout.
    fn press_question(&mut self, target: Target) {
        let Some(question) = self.form.question else {
            return;
        };
        match target {
            Target::Confirm => self.act(Action::Power {
                action: question,
                confirmed: true,
            }),
            Target::Cancel => self.form.close_question(&self.state),
            Target::Power(action) => {
                self.form.focus = Focus::Power(action);
                if let Some(action) = self.form.press_power(action, &self.state) {
                    self.act(action);
                }
            }
            Target::Layout if self.state.layouts.len() > 1 => {
                self.form.focus = Focus::Layout;
                self.act(Action::NextLayout);
            }
            _ => {}
        }
    }

    /// Switch to the next configured layout: the keymap is compiled again
    /// (what is typed from now on, and the "Typed with the … layout." line,
    /// follow), and reclogind is told. It remembers nothing: the layout
    /// lasts until the greeter is restarted.
    fn next_layout(&mut self) {
        let layouts = &self.state.layouts;
        if layouts.len() < 2 {
            return;
        }
        let at = layouts
            .iter()
            .position(|l| l.name == self.state.current_layout)
            .unwrap_or(0);
        let next = layouts[(at + 1) % layouts.len()].clone();
        self.input.set_layout(&next);
        self.state.current_layout = next.name.clone();
        self.send(&ToDaemon::SetLayout { name: next.name });
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::SwitchConsole(number) => self.send(&ToDaemon::SwitchConsole { number }),
            Action::ChooseSession(id) => self.send(&ToDaemon::ChooseSession { id }),
            Action::NextLayout => self.next_layout(),
            Action::Power { action, confirmed } => {
                self.form.question = None;
                self.form.focus = Focus::Power(action);
                self.form.notice = None;
                self.send(&ToDaemon::Power { action, confirmed });
            }
            Action::Cancel => {
                // Once per attempt. The form stays busy (no new `begin` can
                // go out) and says it is waiting until reclogind answers with
                // `auth_result`, once the attempt has really ended.
                if self.form.cancelling {
                    return;
                }
                self.pending = None;
                self.form.cancelling = true;
                self.form.busy = true;
                self.form.talk.asking = None;
                self.form.clear_password();
                self.form.notice = Some((MessageKind::Info, "Cancelling…".into()));
                self.send(&ToDaemon::Cancel);
            }
            Action::Submit => {
                if let Some((prompt_id, _)) = self.form.talk.asking {
                    let secret = Secret::copy_from(&self.form.password);
                    self.form.answered();
                    self.send_answer(prompt_id, &secret);
                    return;
                }
                let Some(goal) = self.form.goal(&self.state) else {
                    return;
                };
                self.form.notice = None;
                self.form.talk = form::Talk::default();
                if let Goal::Return(user) = goal {
                    // reclogind answers with `auth_result`: ok when the
                    // session came to the front, else why not.
                    self.form.busy = true;
                    self.form.returning = true;
                    self.send(&ToDaemon::ReturnTo { user });
                    return;
                }
                let Goal::Begin(who) = goal else {
                    return;
                };
                let password = matches!(who, Who::User { .. } | Who::TypedUser { .. });
                let secret = password.then(|| Secret::copy_from(&self.form.password));
                self.form.clear_password();
                self.form.busy = true;
                if let Some(id) = &self.form.session {
                    self.send(&ToDaemon::ChooseSession { id: id.clone() });
                }
                self.send(&ToDaemon::Begin { who });
                self.pending = secret;
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
