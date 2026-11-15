//! The X server of an X session (stage 3), in the session worker.
//!
//! After the PAM session is open and the console handed to the user,
//! [`start`] starts the system's X server as the user on that console, with
//! the console as its standard input and controlling terminal (the setuid
//! wrapper `Xorg.wrap` only lets a user start the server from a console), and
//! waits until the server itself writes the display number it took to
//! `-displayfd`. The access key is a fresh MIT-MAGIC-COOKIE-1 in an X
//! authority file written here, as the user, in the binary format of libXau
//! (`xauth` and `xinit` are not needed). When the session's command ends,
//! [`XServer::stop`] ends the server and removes the file.
//!
//! The server is started before `reclogind` is told the session is ready,
//! so a server that does not start can still be reported on the login
//! screen. It switches to its console by itself; `reclogind` pauses the
//! greeter on that switch as on any other.
//!
//! What the server's source says (XLibre 25.1.11), which the choices below
//! rest on:
//! - `hw/xfree86/xorg-wrapper.c`: with `allowed_users=console` (the default)
//!   a user may start the server only when standard input, output or error
//!   is a console (character device major 4).
//! - `hw/xfree86/os-support/linux/lnx_init.c`: without `-keeptty` the server
//!   calls `setpgid` and `setsid` to detach from its terminal; it turns
//!   `-keeptty` on by itself when one of its standard descriptors is the
//!   console it runs on. Ours is, and it is already a session leader with
//!   that console as its controlling terminal, so `-keeptty` is given to
//!   say what happens anyway, without relying on that detection.
//! - `hw/xfree86/common/xf86Init.c`, `Xorg.man`: `-novtswitch` keeps the
//!   server from switching to "the VT that was active when the server
//!   started" at reset and at exit. `reclogind` decides which console is
//!   shown after a session; without it every reset (when the last client
//!   leaves; XLibre refuses `-noreset`) and the exit would show the login
//!   console in the middle of `reclogind`'s own switching.
//! - `os/connection.c`: with `-displayfd` and no display number the server
//!   takes the first free one and writes it, with a newline, to that
//!   descriptor once it is ready for clients (`NotifyParentProcess`).
//! - `os/auth.c`: the server loads every MIT-MAGIC-COOKIE-1 entry of its
//!   `-auth` file whatever its address and number, and reads the file
//!   again at each reset.
//! - `hw/xfree86/common/xf86Helper.c`: a server without root rights writes
//!   its log to `$XDG_DATA_HOME/xorg/Xorg.N.log`, else
//!   `~/.local/share/xorg/Xorg.N.log`; with root rights to `/var/log`.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reclogin::log;
use reclogin::wire::Channel;
use zeroize::Zeroize;

use crate::session::{self, Account};
use crate::worker::{next_signal, receive, Command};

const WHO: &str = "reclogind (session worker)";

/// The lines for the screen.
pub const NO_SERVER: &str = "No X server is installed.";
pub const NOT_STARTED: &str = "The X server did not start.";

/// The X server the display managers start; on AcornOS a link to the
/// `Xorg` script, which runs the setuid wrapper `/usr/libexec/Xorg.wrap`.
const SYSTEM_X: &str = "/usr/bin/X";
/// Looked for on the worker's `PATH` when `/usr/bin/X` is missing.
const XORG: &str = "Xorg";
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// How long the server has to report that it is ready.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the server has to end after `SIGTERM` before `SIGKILL`.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The descriptor the server writes its display number to.
const DISPLAY_FD: i32 = 3;

/// libXau's `FamilyLocal`.
const FAMILY_LOCAL: u16 = 256;
const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
const COOKIE_LEN: usize = 16;

const TIOCSCTTY: libc::c_ulong = 0x540E;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0;

/// The X server program: `/usr/bin/X` when it exists, else `Xorg` on the
/// path. `None`: no X server is installed.
pub fn program() -> Option<PathBuf> {
    let runnable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    let system = Path::new(SYSTEM_X);
    if runnable(system) {
        return Some(system.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| DEFAULT_PATH.into());
    std::env::split_paths(&path)
        .map(|dir| dir.join(XORG))
        .find(|p| runnable(p))
}

/// Why [`start`] gave up.
pub enum StartError {
    /// The line for the screen; the reason is in the log.
    Failed(&'static str),
    /// `reclogind` cancelled or went away, or the worker was told to stop.
    Cancelled,
}

// ---------------------------------------------------------------------------
// the access key

/// Do `f` with the user's file-system ids: files it creates are the user's,
/// and it may only do what the user may. The worker is single-threaded, so
/// nothing else runs with these ids meanwhile.
fn as_user<T>(
    uid: libc::uid_t,
    gid: libc::gid_t,
    f: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    // setfsuid/setfsgid return the previous id and report no error; asking
    // again shows whether the change took.
    let took = unsafe {
        libc::setfsgid(gid);
        libc::setfsuid(uid);
        libc::setfsgid(gid) as libc::gid_t == gid && libc::setfsuid(uid) as libc::uid_t == uid
    };
    let result = if took {
        f()
    } else {
        Err(io::Error::other("cannot take the user's file-system ids"))
    };
    unsafe {
        libc::setfsuid(0);
        libc::setfsgid(0);
    }
    result
}

/// Fill `buf` from the kernel's random source.
fn random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe { libc::getrandom(buf[done..].as_mut_ptr().cast(), buf.len() - done, 0) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        done += n as usize;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The host name as clients look it up (`gethostname`), for the entry's
/// address.
fn host_name() -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(buf[..len].to_vec())
}

/// The X authority file: one entry, written by the worker as the user.
struct AuthFile {
    path: PathBuf,
    file: File,
    host: Vec<u8>,
    cookie: [u8; COOKIE_LEN],
    uid: libc::uid_t,
    gid: libc::gid_t,
    removed: bool,
}

impl AuthFile {
    /// A new file with a fresh cookie, mode 0600, the user's: in the user's
    /// runtime folder, else a private file in `/tmp`.
    fn create(account: &Account, runtime_dir: Option<&str>) -> io::Result<AuthFile> {
        let mut cookie = [0u8; COOKIE_LEN];
        random(&mut cookie)?;
        let host = host_name()?;
        let mut places: Vec<(PathBuf, &str)> = Vec::new();
        if let Some(dir) = runtime_dir.filter(|d| d.starts_with('/')) {
            places.push((PathBuf::from(dir), ""));
        }
        let tmp_prefix = format!("{}-", account.uid);
        places.push((PathBuf::from("/tmp"), tmp_prefix.as_str()));
        let mut last = io::Error::other("nowhere to write the X authority file");
        for (dir, prefix) in places {
            let mut tag = [0u8; 8];
            random(&mut tag)?;
            let path = dir.join(format!("reclogin-xauth-{prefix}{}", hex(&tag)));
            let opened = as_user(account.uid, account.gid, || {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)
            });
            match opened {
                Ok(file) => {
                    return Ok(AuthFile {
                        path,
                        file,
                        host,
                        cookie,
                        uid: account.uid,
                        gid: account.gid,
                        removed: false,
                    })
                }
                Err(e) => {
                    log(
                        WHO,
                        format_args!("X authority file {}: {e}", path.display()),
                    );
                    last = e;
                }
            }
        }
        cookie.zeroize();
        Err(last)
    }

    /// Write the file's one entry for display `number` (empty before the
    /// server has said which one; the server ignores the number). libXau's
    /// format (`XauWriteAuth`): the family as a big-endian 16-bit number,
    /// then address, number, name and data, each as a big-endian 16-bit
    /// length and that many bytes.
    fn write(&self, number: &str) -> io::Result<()> {
        let mut entry = Vec::with_capacity(64 + self.host.len());
        entry.extend_from_slice(&FAMILY_LOCAL.to_be_bytes());
        for field in [
            &self.host[..],
            number.as_bytes(),
            COOKIE_NAME,
            &self.cookie[..],
        ] {
            entry.extend_from_slice(&(field.len() as u16).to_be_bytes());
            entry.extend_from_slice(field);
        }
        let written = self
            .file
            .set_len(0)
            .and_then(|()| self.file.write_all_at(&entry, 0))
            .and_then(|()| self.file.sync_data());
        entry.zeroize();
        written
    }

    fn remove(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        if let Err(e) = as_user(self.uid, self.gid, || std::fs::remove_file(&self.path)) {
            log(WHO, format_args!("removing {}: {e}", self.path.display()));
        }
    }
}

impl Drop for AuthFile {
    fn drop(&mut self) {
        self.cookie.zeroize();
    }
}

// ---------------------------------------------------------------------------
// the server

/// A running X server.
pub struct XServer {
    pid: libc::pid_t,
    /// `:N`, once the server said `N`.
    display: String,
    auth: AuthFile,
    /// The read end of the server's standard output and error; `None` once
    /// it closed.
    output: Option<OwnedFd>,
    /// An unfinished line of the server's output.
    partial: Vec<u8>,
    /// The wait status, once reaped.
    status: Option<i32>,
    /// The console device (the session's [`session`] `Tty` owns it and
    /// outlives this).
    tty: RawFd,
    stopped: bool,
}

impl XServer {
    /// `:N`, for `DISPLAY`.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The server's process id, also its process group (it is started in a
    /// session of its own).
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The authority file, for `XAUTHORITY`.
    pub fn auth_path(&self) -> &Path {
        &self.auth.path
    }

    /// Reap the server if it has ended.
    fn reap(&mut self) -> Option<i32> {
        if self.status.is_none() {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if rc == self.pid {
                self.status = Some(status);
            } else if rc < 0 {
                self.status = Some(-1);
            }
        }
        self.status
    }

    /// Pass what the server wrote to the system log, a line at a time.
    fn drain(&mut self) {
        let Some(fd) = &self.output else {
            return;
        };
        let mut buf = [0u8; 4096];
        let closed = loop {
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.partial.extend_from_slice(&buf[..n as usize]);
                continue;
            }
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // 0: the server closed it; an error (EAGAIN): nothing more now.
            break n == 0;
        };
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            log_line(&line[..line.len() - 1]);
        }
        if (closed || self.partial.len() > 4096) && !self.partial.is_empty() {
            let rest = std::mem::take(&mut self.partial);
            log_line(&rest);
        }
        if closed {
            self.output = None;
        }
    }

    /// End the server: `SIGTERM`, `SIGKILL` after a few seconds, then the
    /// authority file is removed. Called once the session's command ended,
    /// and when the server did not start.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if self.reap().is_none() {
            unsafe {
                libc::kill(-self.pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + STOP_TIMEOUT;
            while self.reap().is_none() && Instant::now() < deadline {
                self.drain();
                std::thread::sleep(Duration::from_millis(100));
            }
            if self.reap().is_none() {
                log(WHO, "the X server did not end; killing it");
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                    let mut status = 0;
                    libc::waitpid(self.pid, &mut status, 0);
                    self.status = Some(status);
                    // It had no chance to put its console back.
                    libc::ioctl(self.tty, KDSETMODE as _, KD_TEXT as libc::c_long);
                }
            }
        }
        self.drain();
        if let Some(status) = self.status {
            log(
                WHO,
                format_args!("the X server ended: {}", describe_status(status)),
            );
        }
        self.auth.remove();
    }

    /// Wait until the session's process (`session_pid`) exits, ending it
    /// when asked to or when the X server ends first. The X counterpart of
    /// the Wayland path's wait; it also passes the server's output on.
    pub fn wait_for_end(
        &mut self,
        session_pid: libc::pid_t,
        channel: &Channel,
        signals: &OwnedFd,
    ) -> i32 {
        let mut channel_open = true;
        loop {
            if let Some(status) = session::reap(session_pid) {
                return status;
            }
            if let Some(status) = self.reap() {
                log(
                    WHO,
                    format_args!(
                        "the X server ended ({}); ending the session",
                        describe_status(status)
                    ),
                );
                return session::end(session_pid);
            }
            let mut fds = [
                pollfd(signals.as_raw_fd()),
                pollfd(if channel_open {
                    channel.as_raw_fd()
                } else {
                    -1
                }),
                pollfd(self.output.as_ref().map_or(-1, |fd| fd.as_raw_fd())),
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return session::end(session_pid);
            }
            if fds[2].revents != 0 {
                self.drain();
            }
            let mut stop = false;
            if fds[0].revents != 0 {
                while let Some(signal) = next_signal(signals) {
                    if signal != libc::SIGCHLD {
                        log(WHO, format_args!("ending the session on signal {signal}"));
                        stop = true;
                    }
                }
            }
            if fds[1].revents != 0 {
                match receive(channel) {
                    Ok(Some(Command::Cancel)) => stop = true,
                    Ok(Some(_)) => {}
                    // The session outlives reclogind (`worker.rs`).
                    Ok(None) | Err(_) => {
                        log(WHO, "reclogind went away; the session goes on");
                        channel_open = false;
                    }
                }
            }
            if stop {
                return session::end(session_pid);
            }
        }
    }
}

impl Drop for XServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn log_line(line: &[u8]) {
    log(
        WHO,
        format_args!("X server: {}", String::from_utf8_lossy(line)),
    );
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// A wait status in words, for the log.
fn describe_status(status: i32) -> String {
    if status == -1 {
        "not ours to wait for".into()
    } else if libc::WIFEXITED(status) {
        format!("exit code {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

fn open_tty0() -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open("/dev/tty0")
}

/// The console shown now.
fn active_console() -> Option<u32> {
    let tty0 = open_tty0().ok()?;
    let mut state = VtStat::default();
    let rc = unsafe { libc::ioctl(tty0.as_raw_fd(), VT_GETSTATE as _, &mut state) };
    (rc == 0).then_some(u32::from(state.v_active))
}

/// A server that did not start may have left its console shown: show the
/// one that was shown before it (the login console), so the greeter's
/// message is seen.
fn show_again(console: u32, before: Option<u32>) {
    let Some(before) = before.filter(|&b| b != console) else {
        return;
    };
    if active_console() != Some(console) {
        return;
    }
    if let Ok(tty0) = open_tty0() {
        if unsafe { libc::ioctl(tty0.as_raw_fd(), VT_ACTIVATE as _, before as libc::c_long) } < 0 {
            log(
                WHO,
                format_args!(
                    "showing console {before} again: {}",
                    io::Error::last_os_error()
                ),
            );
        }
    }
}

/// A pipe with both ends closed on exec; the read end non-blocking.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let flags = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((read, write))
}

/// Start the X server as `account` on `console` (whose device `tty` is
/// already the user's) with `env` (the session's environment), and wait
/// until it reports its display number. Watches `channel` and `signals`
/// for a cancel meanwhile.
pub fn start(
    account: &Account,
    console: u32,
    tty: RawFd,
    env: &[CString],
    channel: &Channel,
    signals: &OwnedFd,
) -> Result<XServer, StartError> {
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        log(WHO, format_args!("starting the X server: {what}: {e}"));
        StartError::Failed(NOT_STARTED)
    };
    let Some(program) = program() else {
        log(WHO, "no /usr/bin/X and no Xorg on the path");
        return Err(StartError::Failed(NO_SERVER));
    };

    let runtime_dir = env
        .iter()
        .filter_map(|pair| pair.to_str().ok())
        .find_map(|pair| pair.strip_prefix("XDG_RUNTIME_DIR="))
        .filter(|dir| !dir.is_empty());
    let auth =
        AuthFile::create(account, runtime_dir).map_err(|e| failed("the X authority file", &e))?;
    auth.write("")
        .map_err(|e| failed("writing the X authority file", &e))?;
    // From here `auth` is removed on every failure (`XServer::stop`, or
    // `remove` below before there is a server).
    let mut auth = auth;

    let made = (|| -> io::Result<_> {
        let (ready_r, ready_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let bad = |_| io::Error::from(io::ErrorKind::InvalidInput);
        let args = [
            program.to_string_lossy().into_owned(),
            format!("vt{console}"),
            "-nolisten".into(),
            "tcp".into(),
            "-keeptty".into(),
            "-novtswitch".into(),
            "-auth".into(),
            auth.path.to_string_lossy().into_owned(),
            "-displayfd".into(),
            DISPLAY_FD.to_string(),
        ];
        let argv: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect::<Result<_, _>>()
            .map_err(bad)?;
        let home = CString::new(account.home.as_str()).map_err(bad)?;
        Ok((ready_r, ready_w, out_r, out_w, argv, home))
    })();
    let (ready_r, ready_w, out_r, out_w, argv, home) = match made {
        Ok(made) => made,
        Err(e) => {
            auth.remove();
            return Err(failed("preparing", &e));
        }
    };
    log(
        WHO,
        format_args!(
            "starting the X server: {}",
            argv.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        ),
    );
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    let (uid, gid) = (account.uid, account.gid);
    let before = active_console();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        auth.remove();
        return Err(failed("fork", &error));
    }
    if pid == 0 {
        // The child: only system calls. The worker's descriptors 0 to 3
        // (standard input, output, error, the socket) are open, so the
        // pipes and the console device are above 3 and the `dup2`s below
        // cannot overwrite one another.
        unsafe {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            if libc::setsid() < 0
                || libc::ioctl(tty, TIOCSCTTY as _, 0 as libc::c_long) < 0
                || libc::dup2(tty, 0) < 0
                || libc::dup2(out_w.as_raw_fd(), 1) < 0
                || libc::dup2(out_w.as_raw_fd(), 2) < 0
                || libc::dup2(ready_w.as_raw_fd(), DISPLAY_FD) < 0
            {
                libc::_exit(126);
            }
            libc::syscall(
                libc::SYS_close_range,
                (DISPLAY_FD + 1) as u32,
                u32::MAX,
                0u32,
            );
            // The groups stay the worker's (initgroups and PAM's credential
            // modules), as for the desktop.
            if libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
                || (uid != 0 && libc::setuid(0) == 0)
            {
                libc::_exit(126);
            }
            if libc::chdir(home.as_ptr()) != 0 {
                libc::chdir(c"/".as_ptr());
            }
            libc::execve(argv[0].as_ptr(), argv_p.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    drop(ready_w);
    drop(out_w);

    let mut server = XServer {
        pid,
        display: String::new(),
        auth,
        output: Some(out_r),
        partial: Vec::new(),
        status: None,
        tty,
        stopped: false,
    };

    let outcome = wait_until_ready(&mut server, &ready_r, channel, signals);
    match outcome {
        Ok(number) => {
            server.display = format!(":{number}");
            if let Err(e) = server.auth.write(&number) {
                server.stop();
                show_again(console, before);
                return Err(failed("writing the X authority file", &e));
            }
            log(
                WHO,
                format_args!("the X server is ready on display {}", server.display),
            );
            Ok(server)
        }
        Err(error) => {
            if let StartError::Failed(_) = error {
                log(
                    WHO,
                    format_args!(
                        "the X server did not start; its own lines are above (\"X server: ...\") \
                         and in its log, ~/.local/share/xorg/Xorg.N.log of {} when it ran without root",
                        account.name
                    ),
                );
            }
            server.stop();
            show_again(console, before);
            Err(error)
        }
    }
}

/// Wait for the display number on `ready`. Passes the server's output on
/// meanwhile.
fn wait_until_ready(
    server: &mut XServer,
    ready: &OwnedFd,
    channel: &Channel,
    signals: &OwnedFd,
) -> Result<String, StartError> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut said: Vec<u8> = Vec::new();
    let mut ready_open = true;
    loop {
        if let Some(line) = said
            .iter()
            .position(|&b| b == b'\n')
            .map(|end| &said[..end])
        {
            let number = String::from_utf8_lossy(line).trim().to_string();
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
                return Ok(number);
            }
            log(
                WHO,
                format_args!("the X server reported display {number:?}"),
            );
            return Err(StartError::Failed(NOT_STARTED));
        }
        if let Some(status) = server.reap() {
            server.drain();
            log(
                WHO,
                format_args!(
                    "the X server ended while starting: {}",
                    describe_status(status)
                ),
            );
            return Err(StartError::Failed(NOT_STARTED));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            log(
                WHO,
                format_args!(
                    "the X server did not report a display within {} seconds",
                    READY_TIMEOUT.as_secs()
                ),
            );
            return Err(StartError::Failed(NOT_STARTED));
        }
        let mut fds = [
            pollfd(if ready_open { ready.as_raw_fd() } else { -1 }),
            pollfd(server.output.as_ref().map_or(-1, |fd| fd.as_raw_fd())),
            pollfd(signals.as_raw_fd()),
            pollfd(channel.as_raw_fd()),
        ];
        // At most a second, so that a server whose end was missed is reaped.
        let timeout = left.min(Duration::from_secs(1)).as_millis() as libc::c_int;
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout.max(1)) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log(WHO, format_args!("waiting for the X server: {error}"));
            return Err(StartError::Failed(NOT_STARTED));
        }
        if fds[0].revents != 0 {
            let mut buf = [0u8; 32];
            let got = unsafe { libc::read(ready.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if got > 0 {
                said.extend_from_slice(&buf[..got as usize]);
            } else if got == 0 {
                // Closed without a number: the server is ending (or never
                // ran); its end is reaped above.
                ready_open = false;
            }
        }
        if fds[1].revents != 0 {
            server.drain();
        }
        if fds[2].revents != 0 {
            while let Some(signal) = next_signal(signals) {
                if signal != libc::SIGCHLD {
                    log(
                        WHO,
                        format_args!("stopping the X server on signal {signal}"),
                    );
                    return Err(StartError::Cancelled);
                }
            }
        }
        if fds[3].revents != 0 {
            match receive(channel) {
                Ok(Some(Command::Cancel)) | Ok(None) | Err(_) => {
                    log(WHO, "cancelled while the X server started");
                    return Err(StartError::Cancelled);
                }
                Ok(Some(other)) => {
                    log(WHO, format_args!("unexpected {} ignored", other.kind()));
                }
            }
        }
    }
}
