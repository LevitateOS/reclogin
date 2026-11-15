//! The greeter's own part of its lock-down, done first thing, before it
//! reads anything from the socket or any input.
//!
//! The rest (the account, no capabilities, no network, limits, Landlock,
//! nothing inherited) is applied by reclogind between fork and exec
//! (`reclogind/sandbox.rs`), where the greeter cannot skip it. Here:
//!
//! - never run as root;
//! - not dumpable: exec resets this flag, so only the program itself can
//!   set it. It keeps processes of the same account from tracing it or
//!   reading its memory through `/proc`. Core dumps are off as well
//!   (reclogind set their limit to zero), since a core could hold a typed
//!   password;
//! - refuse to run without no-new-privileges, the sign that reclogind's
//!   lock-down was applied, unless started with `--no-sandbox`.
//! - [`no_new_sockets`]: a seccomp filter that refuses every new socket,
//!   installed by `run` right after `check` (not with `--no-sandbox`, not
//!   for `--render-png`).

use anyhow::{bail, Result};

pub fn check(locked: bool) -> Result<()> {
    if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 } {
        bail!("the greeter must not run as root");
    }
    if !locked {
        return Ok(());
    }
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0) } != 0 {
        bail!(
            "marking the greeter not dumpable: {}",
            std::io::Error::last_os_error()
        );
    }
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        bail!("started without its lock-down: reclogind starts it, or pass --no-sandbox");
    }
    Ok(())
}

/// `seccomp_data` offsets: the system call number and the architecture.
const SECCOMP_NR: u32 = 0;
const SECCOMP_ARCH: u32 = 4;
/// `AUDIT_ARCH_X86_64`.
const ARCH_X86_64: u32 = 0xC000_003E;
/// System call numbers at or above this are the x32 ABI.
const X32_BIT: u32 = 0x4000_0000;

/// Refuse every new socket: `socket`, `socketpair`, `connect`, `bind`,
/// `listen`, `accept` and `accept4` fail with `EACCES`. The network
/// namespace already has no network; this also closes the Unix sockets in
/// the file system (the system bus, the log sockets) and abstract ones on
/// kernels whose Landlock does not. `sendmsg` and `recvmsg` stay allowed, so
/// the socket inherited from reclogind keeps working, and so does every
/// other call. Another architecture's calls (the 32-bit `int 0x80` entry,
/// x32) end the process: they could reach `socketcall`.
///
/// x86_64 only (the supported target). Needs no-new-privileges, which
/// reclogind set (`check` refuses to run locked without it). Installed by
/// `run` after `check` and before anything is read from the socket or input;
/// not with `--no-sandbox` and not for `--render-png`.
pub fn no_new_sockets() -> Result<()> {
    use libc::{sock_filter, sock_fprog};
    const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const JGE_K: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
    const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    let stmt = |code: u16, k: u32| sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code: u16, k: u32, jt: u8, jf: u8| sock_filter { code, jt, jf, k };
    let refused = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
    ];
    let mut filter = vec![
        stmt(LD_W_ABS, SECCOMP_ARCH),
        jump(JEQ_K, ARCH_X86_64, 1, 0),
        stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS),
        stmt(LD_W_ABS, SECCOMP_NR),
        jump(JGE_K, X32_BIT, 0, 1),
        stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS),
    ];
    // Each refused number jumps to the EACCES return after the list; the
    // last one falls through to "allow" when it does not match.
    let n = refused.len();
    for (i, nr) in refused.iter().enumerate() {
        // Instructions left after this one before the EACCES return: the
        // remaining comparisons and the allow.
        let to_refuse = (n - i) as u8;
        filter.push(jump(JEQ_K, *nr as u32, to_refuse, 0));
    }
    filter.push(stmt(RET_K, libc::SECCOMP_RET_ALLOW));
    filter.push(stmt(
        RET_K,
        libc::SECCOMP_RET_ERRNO | (libc::EACCES as u32 & libc::SECCOMP_RET_DATA),
    ));
    let program = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &program as *const sock_fprog as libc::c_ulong,
            0,
            0,
        )
    };
    if rc != 0 {
        bail!(
            "refusing new sockets (seccomp): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
