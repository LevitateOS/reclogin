//! The greeter's lock-down.
//!
//! All of it is applied by reclogind in the greeter's process between fork
//! and exec, so the greeter program never runs a single instruction
//! unconfined and cannot skip any part of it. What needs memory or file
//! names (the Landlock rules, the limits) is prepared here in reclogind
//! before the fork; in the child only system calls that are safe between
//! fork and exec are made.
//!
//! The one thing the greeter does itself is marking its process not
//! dumpable (`reclogin-greeter/sandbox.rs`), because exec resets that flag.
//!
//! In the child, in this order:
//!
//! 1. every descriptor from 4 up is marked close-on-exec (0 to 2 are
//!    standard input from `/dev/null` and reclogind's own output and error,
//!    which go to the system log; 3 is the socket). The environment is
//!    emptied by the caller. Always, even with `--no-sandbox`.
//! 2. as root: a new, empty network namespace; core dumps off; limits on
//!    processes and open files; the capability bounding set and the ambient
//!    set cleared.
//! 3. the account change (no supplementary groups), then the inheritable
//!    set cleared so that no capability is left anywhere.
//! 4. no-new-privileges, then Landlock.
//!
//! Steps 2 and 4's Landlock and step 3's clearing are what `--no-sandbox`
//! turns off.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use reclogin::log;

const WHO: &str = "reclogind";

/// Open files the greeter may have: the socket, the display, its input
/// devices (one each, also counted once more inside libinput), libinput's
/// and the display's own descriptors, and a font or keyboard file being
/// read. A machine with many input devices stays far below this.
pub const OPEN_FILES: libc::rlim_t = 256;
/// Processes and threads of the account `reclogin` together. The greeter
/// is one process with one thread; libraries that start a helper thread
/// still fit.
pub const PROCESSES: libc::rlim_t = 32;

/// `close_range` flag: mark close-on-exec instead of closing (Linux 5.11).
const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

// Landlock, from <linux/landlock.h>.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
const FS_EXECUTE: u64 = 1 << 0;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
/// Every file-system right of ABI 1 (execute to make-symlink).
const FS_ABI1: u64 = (1 << 13) - 1;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
/// The rights a rule on a file (not a folder) may carry.
const FS_FILE_RIGHTS: u64 = FS_EXECUTE | (1 << 1) | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// What may be done under a path.
#[derive(Clone, Copy)]
enum Access {
    /// Read files and list folders, everywhere below.
    Read,
    /// List the folder only; its files cannot be opened.
    List,
    /// Read and execute this one file (the program and its loader).
    Run,
}

impl Access {
    fn rights(self) -> u64 {
        match self {
            Access::Read => FS_READ_FILE | FS_READ_DIR,
            Access::List => FS_READ_DIR,
            Access::Run => FS_READ_FILE | FS_EXECUTE,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::List => "list",
            Access::Run => "run",
        }
    }
}

/// Prepared in reclogind before the fork. Keep it alive until the child
/// has been spawned.
pub struct Prepared {
    landlock: Option<OwnedFd>,
    network_namespace: bool,
}

/// The plain numbers the child needs; safe to copy into the closure that
/// runs between fork and exec.
#[derive(Clone, Copy)]
pub struct Raw {
    landlock: RawFd,
    network_namespace: bool,
}

impl Prepared {
    pub fn raw(&self) -> Raw {
        Raw {
            landlock: self.landlock.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            network_namespace: self.network_namespace,
        }
    }
}

/// Say once, not at every restart, what the greeter is allowed.
static TOLD: AtomicBool = AtomicBool::new(false);

/// Prepare the lock-down for `program`, which reads its brand data from
/// `brand_dir`.
pub fn prepare(program: &Path, brand_dir: &Path) -> Prepared {
    let first = !TOLD.swap(true, Ordering::Relaxed);
    let network_namespace = Path::new("/proc/self/ns/net").exists();
    if !network_namespace && first {
        log(
            WHO,
            "the kernel has no network namespaces: the greeter runs without that limit",
        );
    }
    let landlock = match landlock_ruleset(program, brand_dir, first) {
        Ok(fd) => Some(fd),
        Err(reason) => {
            log(
                WHO,
                format_args!("the greeter runs without the Landlock file limit: {reason}"),
            );
            None
        }
    };
    Prepared {
        landlock,
        network_namespace,
    }
}

/// The paths the greeter may use, and how. Paths that do not exist on this
/// system are left out when the rules are made.
///
/// - its brand folder, and `../fonts` beside it (where `brand.rs` looks
///   for fonts second);
/// - keyboard layouts: `/usr/share/X11/xkb`, and `/etc/xkb` where
///   libxkbcommon looks for local additions;
/// - the clock's time zone: `/etc/localtime` and `/usr/share/zoneinfo`;
/// - libinput's device list: `/dev/input` (listing only; the devices are
///   opened by reclogind), `/sys` and `/run/udev/data` (libinput's udev
///   library reads a device's properties there), `/etc/udev/udev.conf`,
///   and libinput's and libwacom's data in `/usr/share` and `/etc`;
/// - the shared libraries: the usual library folders of both layouts
///   (`/lib`, `/lib64`, `/usr/lib`, `/usr/lib64`, `/usr/local/lib`,
///   `/usr/local/lib64`), glibc's `/etc/ld.so.cache` and
///   `/etc/ld.so.preload`, musl's `/etc/ld-musl-x86_64.path`;
/// - execute on two files only: the greeter program and the loader its
///   ELF header names (`PT_INTERP`: `/lib64/ld-linux-x86-64.so.2` on
///   glibc, `/lib/ld-musl-x86_64.so.1` on musl). Libraries are mapped by
///   the loader after being opened for reading, which needs no execute
///   right.
fn allowed(program: &Path, brand_dir: &Path) -> Vec<(PathBuf, Access)> {
    let mut paths = vec![
        (brand_dir.to_path_buf(), Access::Read),
        (brand_dir.join("..").join("fonts"), Access::Read),
        (PathBuf::from("/usr/share/X11/xkb"), Access::Read),
        (PathBuf::from("/etc/xkb"), Access::Read),
        (PathBuf::from("/etc/localtime"), Access::Read),
        (PathBuf::from("/usr/share/zoneinfo"), Access::Read),
        (PathBuf::from("/dev/input"), Access::List),
        (PathBuf::from("/sys"), Access::Read),
        (PathBuf::from("/run/udev/data"), Access::Read),
        (PathBuf::from("/etc/udev/udev.conf"), Access::Read),
        (PathBuf::from("/usr/share/libinput"), Access::Read),
        (PathBuf::from("/etc/libinput"), Access::Read),
        (PathBuf::from("/usr/share/libwacom"), Access::Read),
        (PathBuf::from("/etc/libwacom"), Access::Read),
    ];
    for dir in [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/local/lib64",
        "/etc/ld.so.cache",
        "/etc/ld.so.preload",
        "/etc/ld-musl-x86_64.path",
    ] {
        paths.push((PathBuf::from(dir), Access::Read));
    }
    paths.push((program.to_path_buf(), Access::Run));
    match interpreter(program) {
        Ok(Some(loader)) => paths.push((loader, Access::Run)),
        Ok(None) => {}
        Err(e) => log(
            WHO,
            format_args!("reading the loader of {}: {e}", program.display()),
        ),
    }
    paths
}

/// The loader named in `program`'s ELF header (`PT_INTERP`), if any. A
/// static program has none.
fn interpreter(program: &Path) -> io::Result<Option<PathBuf>> {
    let file = File::open(program)?;
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0)?;
    // 64-bit, little-endian ELF: the only target (x86_64).
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Ok(None);
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u64_at = |b: &[u8], at: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(bytes)
    };
    let table = u64_at(&header, 0x20);
    let entry_size = u64::from(u16_at(&header, 0x36));
    let entries = u64::from(u16_at(&header, 0x38));
    if entry_size < 56 {
        return Ok(None);
    }
    for i in 0..entries {
        let mut entry = [0u8; 56];
        file.read_exact_at(&mut entry, table + i * entry_size)?;
        const PT_INTERP: u32 = 3;
        if u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]) != PT_INTERP {
            continue;
        }
        let (offset, size) = (u64_at(&entry, 8), u64_at(&entry, 32));
        if size == 0 || size > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "loader name of a strange size",
            ));
        }
        let mut name = vec![0u8; size as usize];
        file.read_exact_at(&mut name, offset)?;
        while name.last() == Some(&0) {
            name.pop();
        }
        return Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(&name))));
    }
    Ok(None)
}

/// The Landlock ruleset: every right the running kernel knows is handled,
/// so everything not allowed by a rule is refused.
fn landlock_ruleset(program: &Path, brand_dir: &Path, tell: bool) -> Result<OwnedFd, String> {
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 1 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ENOSYS) => "the kernel has no Landlock".into(),
            Some(libc::EOPNOTSUPP) => "Landlock is turned off at boot".into(),
            _ => format!("asking for Landlock: {error}"),
        });
    }
    let mut fs = FS_ABI1;
    if abi >= 2 {
        fs |= FS_REFER;
    }
    if abi >= 3 {
        fs |= FS_TRUNCATE;
    }
    if abi >= 5 {
        fs |= FS_IOCTL_DEV;
    }
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: if abi >= 4 {
            NET_BIND_TCP | NET_CONNECT_TCP
        } else {
            0
        },
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(format!(
            "making the Landlock rules: {}",
            io::Error::last_os_error()
        ));
    }
    // Created close-on-exec by the kernel.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

    let mut granted = Vec::new();
    for (path, access) in allowed(program, brand_dir) {
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        let target = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if target < 0 {
            continue; // not on this system
        }
        let target = unsafe { OwnedFd::from_raw_fd(target) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(target.as_raw_fd(), &mut st) } < 0 {
            continue;
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        let mut rights = access.rights() & fs;
        if !is_dir {
            rights &= FS_FILE_RIGHTS;
        }
        if rights == 0 {
            continue;
        }
        let rule = PathBeneathAttr {
            allowed_access: rights,
            parent_fd: target.as_raw_fd(),
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                LANDLOCK_RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        if rc < 0 {
            log(
                WHO,
                format_args!(
                    "Landlock rule for {}: {}",
                    path.display(),
                    io::Error::last_os_error()
                ),
            );
            continue;
        }
        granted.push(format!("{} {}", access.word(), path.display()));
    }
    if tell {
        log(
            WHO,
            format_args!(
                "the greeter's files (Landlock ABI {abi}): {}; nothing writable",
                granted.join(", ")
            ),
        );
    }
    Ok(ruleset)
}

/// The highest descriptor number to mark close-on-exec when the kernel has
/// no `close_range` (read before the fork).
pub fn descriptor_ceiling() -> libc::c_int {
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 1024;
    }
    limit.rlim_cur.min(65_536) as libc::c_int
}

fn check(rc: libc::c_long) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// In the child, after descriptor 3 is in place: nothing but 0 to 3 stays
/// open across exec. Marking instead of closing keeps the standard
/// library's own close-on-exec pipe, which reports a failed exec, working.
///
/// # Safety
/// Only between fork and exec.
pub unsafe fn inherit_nothing(ceiling: libc::c_int) -> io::Result<()> {
    if libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, CLOSE_RANGE_CLOEXEC) == 0 {
        return Ok(());
    }
    for fd in 4..ceiling {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(())
}

/// In the child, still root, before the account change.
///
/// # Safety
/// Only between fork and exec.
pub unsafe fn as_root(raw: &Raw) -> io::Result<()> {
    if raw.network_namespace {
        check(libc::unshare(libc::CLONE_NEWNET).into())?;
    }
    for (resource, value) in [
        (libc::RLIMIT_CORE, 0),
        (libc::RLIMIT_NOFILE, OPEN_FILES),
        (libc::RLIMIT_NPROC, PROCESSES),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        check(libc::setrlimit(resource, &limit).into())?;
    }
    // The bounding set: drop every capability the kernel knows; EINVAL
    // marks the first number past the last one.
    for cap in 0..64 {
        if libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            return Err(error);
        }
    }
    // The ambient set (EINVAL on kernels before 4.3, which have none).
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    if libc::prctl(libc::PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINVAL) {
            return Err(error);
        }
    }
    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAP_VERSION_3: u32 = 0x2008_0522;

/// In the child, after the account change: the account change cleared the
/// permitted and effective sets; clear the inheritable one too and check
/// that nothing is left.
///
/// # Safety
/// Only between fork and exec.
pub unsafe fn no_capabilities() -> io::Result<()> {
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let none = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    check(libc::syscall(
        libc::SYS_capset,
        &mut header as *mut CapHeader,
        none.as_ptr(),
    ))?;
    let mut now = none;
    check(libc::syscall(
        libc::SYS_capget,
        &mut header as *mut CapHeader,
        now.as_mut_ptr(),
    ))?;
    if now
        .iter()
        .any(|d| d.effective != 0 || d.permitted != 0 || d.inheritable != 0)
    {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

/// In the child, after no-new-privileges: the file-system view.
///
/// # Safety
/// Only between fork and exec.
pub unsafe fn restrict_files(raw: &Raw) -> io::Result<()> {
    if raw.landlock < 0 {
        return Ok(());
    }
    check(libc::syscall(
        libc::SYS_landlock_restrict_self,
        raw.landlock,
        0u32,
    ))
}
