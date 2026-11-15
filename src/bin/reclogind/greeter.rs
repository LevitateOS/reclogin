//! Starting the greeter as the account `reclogin`, with no rights.
//!
//! The greeter gets one thing: its end of the socket, as file descriptor 3
//! (beside standard input from `/dev/null` and reclogind's own output and
//! error). Its environment is empty except for the variable naming that
//! descriptor, it has no supplementary groups, and it can never gain rights
//! (no-new-privileges). Its display and input handles come over the socket.
//! The rest of its lock-down (no network, no capabilities, limits, a
//! read-only view of its data through Landlock) is in [`crate::sandbox`],
//! applied here between fork and exec unless `--no-sandbox` was given.

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reclogin::protocol::{Framed, GREETER_SOCKET_ENV, GREETER_SOCKET_FD};
use reclogin::wire::Channel;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::sandbox;

/// How every greeter is started; set once by [`configure`].
pub struct Settings {
    /// The brand folder, which the greeter may read.
    pub brand_dir: PathBuf,
    /// The lock-down of [`crate::sandbox`]; off with `--no-sandbox`.
    pub sandbox: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            brand_dir: PathBuf::from(reclogin::brand::DEFAULT_DIR),
            sandbox: true,
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Set how greeters are started, once, before the first [`start`]. Without
/// it [`start`] uses [`Settings::default`]: the lock-down on.
pub fn configure(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

/// The most messages waiting for a greeter that does not read.
const OUTBOX_MAX: usize = 64;
/// A greeter whose queue has not moved for this long is hung.
const STUCK_AFTER: Duration = Duration::from_secs(5);
/// How often a shown greeter is asked whether it is alive.
const PING_EVERY: Duration = Duration::from_secs(3);
/// How long it has to answer.
const PONG_WITHIN: Duration = Duration::from_secs(10);

pub struct Greeter {
    pub child: Child,
    /// reclogind's end, non-blocking: sending never waits for the greeter.
    pub channel: Channel,
    /// Messages that did not fit in the socket yet, oldest first, with
    /// copies of their handles; sent from the main loop when it can write.
    outbox: VecDeque<(Zeroizing<Vec<u8>>, Vec<OwnedFd>)>,
    /// Since when the oldest waiting message has not moved.
    stuck_since: Option<Instant>,
    next_seq: u64,
    /// The ping not answered yet and when it was sent.
    ping: Option<(u64, Instant)>,
    next_ping: Instant,
}

impl Greeter {
    /// Send `message` with `handles`, or queue it when the socket is full.
    /// An error means the greeter cannot be reached (or its queue is full):
    /// the caller treats it as gone.
    pub fn send<M: Serialize + Framed>(
        &mut self,
        message: &M,
        handles: &[BorrowedFd<'_>],
    ) -> io::Result<()> {
        let bytes = Channel::encode(message, handles.len())?;
        if self.outbox.is_empty() {
            match self.channel.send_packet(&[&bytes], handles) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if self.outbox.len() >= OUTBOX_MAX {
            return Err(io::Error::other("its queue of messages is full"));
        }
        let handles = handles
            .iter()
            .map(|h| h.try_clone_to_owned())
            .collect::<io::Result<Vec<_>>>()?;
        self.outbox.push_back((bytes, handles));
        self.stuck_since.get_or_insert_with(Instant::now);
        Ok(())
    }

    /// Whether messages wait for the socket to take them (poll for writing).
    pub fn wants_write(&self) -> bool {
        !self.outbox.is_empty()
    }

    /// Send what waits, as far as the socket takes it.
    pub fn flush(&mut self) -> io::Result<()> {
        while let Some((bytes, handles)) = self.outbox.front() {
            let borrowed: Vec<BorrowedFd<'_>> = handles.iter().map(|h| h.as_fd()).collect();
            match self.channel.send_packet(&[bytes], &borrowed) {
                Ok(()) => {
                    self.outbox.pop_front();
                    self.stuck_since = (!self.outbox.is_empty()).then(Instant::now);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Why the greeter counts as hung now, if it does: its queue has not
    /// moved for five seconds, or a ping was not answered within ten.
    pub fn hung(&self, now: Instant) -> Option<&'static str> {
        if self
            .stuck_since
            .is_some_and(|t| now.duration_since(t) >= STUCK_AFTER)
        {
            return Some("stopped reading its socket for five seconds");
        }
        if self
            .ping
            .is_some_and(|(_, sent)| now.duration_since(sent) >= PONG_WITHIN)
        {
            return Some("did not answer a ping within ten seconds");
        }
        None
    }

    /// The ping to send now, if one is due (none while one is unanswered).
    pub fn ping_due(&mut self, now: Instant) -> Option<u64> {
        if self.ping.is_some() || now < self.next_ping {
            return None;
        }
        self.next_seq += 1;
        self.ping = Some((self.next_seq, now));
        Some(self.next_seq)
    }

    /// `pong { seq }` arrived.
    pub fn pong(&mut self, seq: u64) {
        if self.ping.is_some_and(|(sent, _)| sent == seq) {
            self.ping = None;
            self.next_ping = Instant::now() + PING_EVERY;
        }
    }

    /// The login console was left: no pings until it is shown again.
    pub fn stop_pinging(&mut self) {
        self.ping = None;
        self.next_ping = Instant::now() + PING_EVERY;
    }

    /// The next moment `tick` has something to check for this greeter;
    /// `pinging`: whether pings are sent now (the login console is shown).
    /// `None`: nothing.
    pub fn next_deadline(&self, pinging: bool) -> Option<Instant> {
        let ping = match self.ping {
            Some((_, sent)) => Some(sent + PONG_WITHIN),
            None => pinging.then_some(self.next_ping),
        };
        let stuck = self.stuck_since.map(|t| t + STUCK_AFTER);
        match (ping, stuck) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

struct Account {
    uid: libc::uid_t,
    gid: libc::gid_t,
}

fn account(name: &str) -> Result<Account> {
    let c_name = CString::new(name).context("account name")?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc))
            .with_context(|| format!("looking up account {name}"));
    }
    if result.is_null() {
        bail!("there is no account {name}");
    }
    if pwd.pw_uid == 0 {
        bail!("account {name} is root; the greeter must not run as root");
    }
    Ok(Account {
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
    })
}

/// Start the greeter `program` as `account_name`, as [`configure`] set.
pub fn start(program: &Path, account_name: &str) -> Result<Greeter> {
    match SETTINGS.get() {
        Some(settings) => start_with(program, account_name, settings),
        None => start_with(program, account_name, &Settings::default()),
    }
}

pub fn start_with(program: &Path, account_name: &str, settings: &Settings) -> Result<Greeter> {
    if !program.is_file() {
        bail!("the greeter program {} is missing", program.display());
    }
    let account = account(account_name)?;
    let (ours, theirs) = Channel::pair().context("making the greeter's socket")?;
    let theirs: OwnedFd = theirs.into_fd();
    let their_fd = theirs.as_raw_fd();
    let (uid, gid) = (account.uid, account.gid);
    let prepared = settings
        .sandbox
        .then(|| sandbox::prepare(program, &settings.brand_dir));
    let locked = prepared.as_ref().map(sandbox::Prepared::raw);
    let ceiling = sandbox::descriptor_ceiling();
    let parent = unsafe { libc::getpid() };

    let mut command = Command::new(program);
    command
        .env_clear()
        .env(GREETER_SOCKET_ENV, GREETER_SOCKET_FD.to_string())
        .stdin(Stdio::null())
        .current_dir("/");
    if !settings.sandbox {
        command.arg("--no-sandbox");
    }
    unsafe {
        command.pre_exec(move || {
            // Only async-signal-safe calls between fork and exec.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            if their_fd == GREETER_SOCKET_FD {
                if libc::fcntl(their_fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(their_fd, GREETER_SOCKET_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            sandbox::inherit_nothing(ceiling)?;
            if let Some(locked) = &locked {
                sandbox::as_root(locked)?;
            }
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if locked.is_some() {
                sandbox::no_capabilities()?;
            }
            // After the account change, which clears the parent-death
            // signal; and if reclogind already died, do not run at all.
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
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(locked) = &locked {
                sandbox::restrict_files(locked)?;
            }
            if libc::setuid(0) == 0 {
                // Rights could be regained: refuse to run.
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("starting {} as {account_name}", program.display()))?;
    drop(theirs);
    drop(prepared);
    let mut greeter = Greeter {
        child,
        channel: ours,
        outbox: VecDeque::new(),
        stuck_since: None,
        next_seq: 0,
        ping: None,
        next_ping: Instant::now() + PING_EVERY,
    };
    if let Err(e) = greeter.channel.set_nonblocking() {
        let _ = greeter.child.kill();
        let _ = greeter.child.wait();
        return Err(e).context("making the greeter's socket non-blocking");
    }
    Ok(greeter)
}
