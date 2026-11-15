//! Starting the greeter as the account `reclogin`, with no rights.
//!
//! The greeter gets one thing: its end of the socket, as file descriptor 3.
//! Its environment is empty except for the variable naming that descriptor,
//! it has no supplementary groups, and it can never gain rights
//! (no-new-privileges). Its display and input handles come over the socket.
//! The greeter's own lock-down (no network, read-only view of its data) is a
//! later stage.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};
use reclogin::protocol::{GREETER_SOCKET_ENV, GREETER_SOCKET_FD};
use reclogin::wire::Channel;

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

pub fn start(program: &Path, account_name: &str) -> Result<Greeter> {
    let account = account(account_name)?;
    let (ours, theirs) = Channel::pair().context("making the greeter's socket")?;
    let theirs: OwnedFd = theirs.into_fd();
    let their_fd = theirs.as_raw_fd();
    let (uid, gid) = (account.uid, account.gid);

    let mut command = Command::new(program);
    command
        .env_clear()
        .env(GREETER_SOCKET_ENV, GREETER_SOCKET_FD.to_string())
        .stdin(Stdio::null())
        .current_dir("/");
    unsafe {
        command.pre_exec(move || {
            // Only async-signal-safe calls between fork and exec.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
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
            if their_fd == GREETER_SOCKET_FD {
                if libc::fcntl(their_fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(their_fd, GREETER_SOCKET_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) != 0
            {
                return Err(io::Error::last_os_error());
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
    Ok(Greeter {
        child,
        channel: ours,
    })
}
