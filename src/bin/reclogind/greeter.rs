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

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use reclogin::protocol::{GREETER_SOCKET_ENV, GREETER_SOCKET_FD};
use reclogin::wire::Channel;

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

pub struct Greeter {
    pub child: Child,
    pub channel: Channel,
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
    Ok(Greeter {
        child,
        channel: ours,
    })
}
