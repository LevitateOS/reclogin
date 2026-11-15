//! The session worker: one root process per login, owning that login's
//! PAM handle from the first question to the end of the session.
//!
//! `reclogind` starts itself again as `reclogind --session-worker` for each
//! login ([`crate::login`] does that), with its end of a private socket pair
//! as file descriptor 3, the way `sddm-helper` and `gdm-session-worker`
//! work. Two reasons: PAM's conversation is synchronous and must not block
//! `reclogind`'s main loop, which keeps serving the greeter; and the login
//! service registers the process that opens the PAM session as the
//! session's first process, which must not be `reclogind` itself.
//!
//! The worker is single-threaded. It ends when the login fails, is
//! cancelled, or its session has ended and been closed.
//!
//! The messages between the worker and `reclogind` are [`ToWorker`] and
//! [`FromWorker`], over the same kind of socket and framing as the greeter's
//! ([`reclogin::wire`]). A secret crosses as [`Secret`], overwritten after use.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use reclogin::log;
use reclogin::protocol::{Framed, MessageKind, PromptKind, Secret, SessionKind};
use reclogin::wire::Channel;
use serde::{Deserialize, Serialize};

use crate::pam::{self, Conversation, Pam};
use crate::session;

/// The hidden switch that makes `reclogind` a session worker.
pub const SWITCH: &str = "--session-worker";

/// The worker's end of the socket.
pub const SOCKET_FD: i32 = 3;

/// The PAM service of a person's login.
const SERVICE: &str = "reclogin";

/// What the screen says when authentication fails and PAM said nothing.
/// It never says which part was wrong.
pub const AUTH_FAILED: &str = "Authentication failed.";
const NOT_CHANGED: &str = "The password was not changed.";
const ACCEPTED: &str = "Current password accepted.";

const WHO: &str = "reclogind (session worker)";

// ---------------------------------------------------------------------------
// the messages

/// The session a login is to start, as `reclogind` validated it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    /// The session file's name without `.desktop`.
    pub id: String,
    pub kind: SessionKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWorker {
    /// First message: log `user` in and start `session`.
    Start { user: String, session: SessionRef },
    /// The answer to the last `Prompt`.
    Answer { secret: Secret },
    /// The answer to `NeedConsole`: the console the session is to have.
    Console { number: u32 },
    /// Abandon the login; or, once the session runs, end it.
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromWorker {
    /// A question from PAM, word for word.
    Prompt { kind: PromptKind, text: String },
    /// A message from PAM (or the worker's own `Success` line), word for word.
    Message { kind: MessageKind, text: String },
    /// The login failed before a session was started. `text` is the line
    /// for the screen; empty when PAM's own messages already said why.
    Failed { text: String },
    /// PAM accepted the person: give the session a console.
    NeedConsole,
    /// The PAM session is open on `console`: show that console. The worker
    /// starts the desktop once the console is shown.
    ReadyToStart { console: u32 },
    /// The desktop's process runs.
    Started,
    /// The session is over and closed. `status` is the desktop process's
    /// wait status, or -1 when it never ran.
    Ended { status: i32 },
}

impl Framed for ToWorker {
    fn handles(&self) -> usize {
        0
    }

    fn kind(&self) -> &'static str {
        match self {
            ToWorker::Start { .. } => "start",
            ToWorker::Answer { .. } => "answer",
            ToWorker::Console { .. } => "console",
            ToWorker::Cancel => "cancel",
        }
    }
}

impl Framed for FromWorker {
    fn handles(&self) -> usize {
        0
    }

    fn kind(&self) -> &'static str {
        match self {
            FromWorker::Prompt { .. } => "prompt",
            FromWorker::Message { .. } => "message",
            FromWorker::Failed { .. } => "failed",
            FromWorker::NeedConsole => "need_console",
            FromWorker::ReadyToStart { .. } => "ready_to_start",
            FromWorker::Started => "started",
            FromWorker::Ended { .. } => "ended",
        }
    }
}

// ---------------------------------------------------------------------------
// the conversation

/// Relays PAM's conversation to `reclogind` and waits for the answers.
pub struct Talk<'a> {
    channel: &'a Channel,
    /// The person cancelled or `reclogind` went away: every further question
    /// is refused, and the worker ends without another word.
    pub cancelled: bool,
    /// PAM sent an error message during the current step: its words replace
    /// the generic failure line.
    pub spoke_error: bool,
    /// Messages held back while the account is checked, so that "Current
    /// password accepted." can come before them when the password expired.
    held: Option<Vec<(MessageKind, String)>>,
}

impl<'a> Talk<'a> {
    fn new(channel: &'a Channel) -> Talk<'a> {
        Talk {
            channel,
            cancelled: false,
            spoke_error: false,
            held: None,
        }
    }

    fn send(&mut self, message: &FromWorker) {
        if let Err(e) = self.channel.send(message, &[]) {
            log(WHO, format_args!("sending {}: {e}", message.kind()));
            self.cancelled = true;
        }
    }

    fn hold(&mut self) {
        self.held = Some(Vec::new());
    }

    /// Send what was held back, in order, and stop holding.
    fn release(&mut self) {
        for (kind, text) in self.held.take().unwrap_or_default() {
            self.send(&FromWorker::Message { kind, text });
        }
    }

    /// A line in the worker's own words, sent at once even while holding.
    fn say(&mut self, kind: MessageKind, text: &str) {
        self.send(&FromWorker::Message {
            kind,
            text: text.into(),
        });
    }
}

impl Conversation for Talk<'_> {
    fn ask(&mut self, kind: PromptKind, text: &str) -> Option<Secret> {
        if self.cancelled {
            return None;
        }
        self.release();
        self.send(&FromWorker::Prompt {
            kind,
            text: text.into(),
        });
        loop {
            if self.cancelled {
                return None;
            }
            match self.channel.recv::<ToWorker>() {
                Ok(Some((ToWorker::Answer { secret }, _))) => return Some(secret),
                Ok(Some((ToWorker::Cancel, _))) | Ok(None) => {
                    self.cancelled = true;
                    return None;
                }
                Ok(Some((other, _))) => {
                    log(WHO, format_args!("unexpected {} ignored", other.kind()));
                }
                Err(e) => {
                    log(WHO, format_args!("from reclogind: {e}"));
                    self.cancelled = true;
                    return None;
                }
            }
        }
    }

    fn tell(&mut self, kind: MessageKind, text: &str) {
        if kind == MessageKind::Error {
            self.spoke_error = true;
        }
        match &mut self.held {
            Some(held) => held.push((kind, text.to_string())),
            None => self.send(&FromWorker::Message {
                kind,
                text: text.to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// the worker's life

pub fn run() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(WHO, format_args!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// The socket from `reclogind`, checked to be one.
fn channel() -> Result<Channel> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("the session worker must run as root");
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(SOCKET_FD, &mut st) } != 0
        || st.st_mode & libc::S_IFMT != libc::S_IFSOCK
    {
        bail!("descriptor {SOCKET_FD} is not reclogind's socket");
    }
    if unsafe { libc::fcntl(SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error()).context("closing the socket on exec");
    }
    Ok(Channel::from_fd(unsafe { OwnedFd::from_raw_fd(SOCKET_FD) }))
}

/// Block the signals the worker waits for and read them from a descriptor.
fn signals() -> Result<OwnedFd> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error()).context("blocking signals");
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("signal descriptor");
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn serve() -> Result<()> {
    let channel = channel()?;
    let signals = signals()?;
    let (user, session) = match channel.recv::<ToWorker>()? {
        Some((ToWorker::Start { user, session }, _)) => (user, session),
        Some((other, _)) => bail!("expected start, got {}", other.kind()),
        None => return Ok(()),
    };
    let fail = |text: &str| channel.send(&FromWorker::Failed { text: text.into() }, &[]);

    // What to run, before anyone types a password for it.
    let plan = match session::read(&session) {
        Ok(plan) => plan,
        Err(text) => {
            fail(&text)?;
            return Ok(());
        }
    };

    let mut pam = match Pam::start(SERVICE, &user, Talk::new(&channel)) {
        Ok(pam) => pam,
        Err(e) => {
            log(WHO, e);
            fail(AUTH_FAILED)?;
            return Ok(());
        }
    };

    // Who you are.
    let rc = pam.authenticate();
    if pam.talker().cancelled {
        return Ok(());
    }
    if rc != pam::SUCCESS {
        log(
            WHO,
            format_args!("authentication refused: {}", pam.describe(rc)),
        );
        let text = failure_text(&mut pam, AUTH_FAILED);
        fail(&text)?;
        return Ok(());
    }

    // Whether you may log in now; an expired password is changed here.
    pam.talker().hold();
    let rc = pam.acct_mgmt();
    if pam.talker().cancelled {
        return Ok(());
    }
    match rc {
        pam::SUCCESS => pam.talker().release(),
        pam::NEW_AUTHTOK_REQD => {
            pam.talker().say(MessageKind::Success, ACCEPTED);
            pam.talker().release();
            pam.talker().spoke_error = false;
            let rc = pam.chauthtok(pam::CHANGE_EXPIRED_AUTHTOK);
            if pam.talker().cancelled {
                return Ok(());
            }
            if rc != pam::SUCCESS {
                log(
                    WHO,
                    format_args!("password not changed: {}", pam.describe(rc)),
                );
                let text = failure_text(&mut pam, NOT_CHANGED);
                fail(&text)?;
                return Ok(());
            }
        }
        _ => {
            pam.talker().release();
            log(WHO, format_args!("account refused: {}", pam.describe(rc)));
            let text = failure_text(&mut pam, AUTH_FAILED);
            fail(&text)?;
            return Ok(());
        }
    }

    // A module may have changed the name; the account is PAM's.
    let name = pam.user().unwrap_or(user);
    let account = match session::Account::lookup(&name) {
        Ok(account) => account,
        Err(e) => {
            log(WHO, format_args!("account {name}: {e}"));
            fail(AUTH_FAILED)?;
            return Ok(());
        }
    };

    channel.send(&FromWorker::NeedConsole, &[])?;
    let console = loop {
        match channel.recv::<ToWorker>()? {
            Some((ToWorker::Console { number }, _)) => break number,
            Some((ToWorker::Cancel, _)) | None => return Ok(()),
            Some((other, _)) => log(WHO, format_args!("unexpected {} ignored", other.kind())),
        }
    };

    session::run(&mut pam, &account, &plan, console, &channel, &signals);
    Ok(())
}

/// The failure line: empty when PAM said why in an error message of its own.
fn failure_text(pam: &mut Pam<Talk<'_>>, generic: &str) -> String {
    if pam.talker().spoke_error {
        String::new()
    } else {
        generic.to_string()
    }
}

/// The next signal waiting in `fd`, if any.
pub fn next_signal(fd: &OwnedFd) -> Option<i32> {
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
