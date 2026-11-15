//! No set-user-ID or set-group-ID file can be made by a guest session
//! (`docs/reclogin-guest.md`, point 1; the independent review's finding 1).
//!
//! Why: a set-user-ID copy of a shell that belongs to uid 60600, left in a
//! place the cleanup does not sweep (a folder another user opened to
//! everyone), would let that user become the next guest during the next
//! guest's session and read that guest's home and X cookie. So the kernel
//! refuses to make such a file for the whole session: the guest's desktop
//! process and its X server process install this filter between `fork` and
//! `exec`, after no-new-privileges, and every process of the session
//! inherits both; neither can be undone.
//!
//! What a guest loses: no set-user-ID or set-group-ID program works inside
//! a guest session (no-new-privileges: `su`, `sudo`, `doas`, `passwd`,
//! `crontab`, `at`, `fusermount`, `pkexec` start without their rights and
//! fail; programs with file capabilities run without them), and no file or
//! folder with either bit can be made or given either bit. The design
//! already gives the guest none of those rights. `openat2` and io_uring are
//! refused as "not implemented" (their arguments cannot be read here);
//! programs fall back to `openat` and plain reads and writes.
//!
//! The rules (x86_64 only, the supported target; any other ABI, the 32-bit
//! `int 0x80` entry or x32, ends the process):
//!
//! | call | number | mode is argument | refused with `EPERM` when |
//! | --- | --- | --- | --- |
//! | `chmod(path, mode)` | 90 | 1 | mode has 04000 or 02000 |
//! | `fchmod(fd, mode)` | 91 | 1 | same |
//! | `fchmodat(dfd, path, mode)` | 268 | 2 | same |
//! | `fchmodat2(dfd, path, mode, flags)` | 452 | 2 | same |
//! | `mkdir(path, mode)` | 83 | 1 | same |
//! | `mkdirat(dfd, path, mode)` | 258 | 2 | same |
//! | `mknod(path, mode, dev)` | 133 | 1 | same |
//! | `mknodat(dfd, path, mode, dev)` | 259 | 2 | same |
//! | `creat(path, mode)` | 85 | 1 | same |
//! | `open(path, flags, mode)` | 2 | 2 | flags (argument 1) have `O_CREAT` or `O_TMPFILE`, and the mode has a set-id bit |
//! | `openat(dfd, path, flags, mode)` | 257 | 3 | flags (argument 2) have `O_CREAT` or `O_TMPFILE`, and the mode has a set-id bit |
//! | `openat2(dfd, path, how, size)` | 437 | behind a pointer | always, with `ENOSYS` |
//! | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | 425, 426, 427 | (io_uring opens and makes folders past this filter) | always, with `ENOSYS` |
//!
//! Every other call is allowed. The kernel reads a mode as `umode_t` (16
//! bits) and flags as `int`, so the lower 32 bits of each 64-bit argument
//! are all that is compared (`seccomp_data.args[i]` at byte 16 + 8 * i; the
//! lower half comes first on x86_64).

use std::io;

use libc::{sock_filter, sock_fprog};

/// `struct seccomp_data`: `nr` at 0, `arch` at 4, `args[i]` at 16 + 8 * i.
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}
/// `AUDIT_ARCH_X86_64`.
const ARCH_X86_64: u32 = 0xC000_003E;
/// System call numbers at or above this are the x32 ABI.
const X32_BIT: u32 = 0x4000_0000;

/// `S_ISUID | S_ISGID`.
const SET_ID: u32 = 0o6000;
/// `O_CREAT` (0o100) and `__O_TMPFILE` (0o20000000), the bit that every
/// `O_TMPFILE` carries.
const CREATING: u32 = 0o100 | 0o2000_0000;

// x86_64 system call numbers (arch/x86/entry/syscalls/syscall_64.tbl).
const NR_OPEN: u32 = 2;
const NR_MKDIR: u32 = 83;
const NR_CREAT: u32 = 85;
const NR_CHMOD: u32 = 90;
const NR_FCHMOD: u32 = 91;
const NR_MKNOD: u32 = 133;
const NR_OPENAT: u32 = 257;
const NR_MKDIRAT: u32 = 258;
const NR_MKNODAT: u32 = 259;
const NR_FCHMODAT: u32 = 268;
const NR_IO_URING_SETUP: u32 = 425;
const NR_IO_URING_ENTER: u32 = 426;
const NR_IO_URING_REGISTER: u32 = 427;
const NR_OPENAT2: u32 = 437;
const NR_FCHMODAT2: u32 = 452;

const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGE_K: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
const JSET_K: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

/// A jump target, resolved when the program is finished.
#[derive(Clone, Copy)]
struct Label(usize);

/// A tiny assembler for classic BPF: instructions whose jumps name labels.
struct Asm {
    code: Vec<(u16, u32, Option<Label>, Option<Label>)>,
    labels: Vec<Option<usize>>,
}

impl Asm {
    fn new() -> Asm {
        Asm {
            code: Vec::new(),
            labels: Vec::new(),
        }
    }

    fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    fn here(&mut self, label: Label) {
        self.labels[label.0] = Some(self.code.len());
    }

    fn stmt(&mut self, code: u16, k: u32) {
        self.code.push((code, k, None, None));
    }

    /// A conditional jump: to `yes` when true, else to `no` (`None`: the
    /// next instruction).
    fn jump(&mut self, code: u16, k: u32, yes: Option<Label>, no: Option<Label>) {
        self.code.push((code, k, yes, no));
    }

    fn finish(self) -> Vec<sock_filter> {
        let offset = |at: usize, label: Option<Label>| -> u8 {
            let Some(label) = label else {
                return 0;
            };
            let target = self.labels[label.0].expect("every label is placed");
            // Classic BPF jumps forward only, by at most 255.
            u8::try_from(target - (at + 1)).expect("the filter is short")
        };
        self.code
            .iter()
            .enumerate()
            .map(|(at, &(code, k, yes, no))| sock_filter {
                code,
                jt: offset(at, yes),
                jf: offset(at, no),
                k,
            })
            .collect()
    }
}

/// The filter, made before `fork` (it allocates) and installed in the child
/// with [`Filter::install`], which only makes system calls.
pub struct Filter {
    program: Vec<sock_filter>,
}

impl Filter {
    pub fn new() -> Filter {
        let mut a = Asm::new();
        let allow = a.label();
        let deny = a.label();
        let nosys = a.label();
        let kill = a.label();

        // Only the x86_64 ABI.
        a.stmt(LD_W_ABS, DATA_ARCH);
        a.jump(JEQ_K, ARCH_X86_64, None, Some(kill));
        a.stmt(LD_W_ABS, DATA_NR);
        a.jump(JGE_K, X32_BIT, Some(kill), None);

        // The calls whose mode is argument 1, those whose mode is argument
        // 2, the two opens, and the ones always refused.
        let mode_1 = a.label();
        let mode_2 = a.label();
        let open = a.label();
        let openat = a.label();
        for nr in [NR_CHMOD, NR_FCHMOD, NR_MKDIR, NR_MKNOD, NR_CREAT] {
            a.jump(JEQ_K, nr, Some(mode_1), None);
        }
        for nr in [NR_FCHMODAT, NR_FCHMODAT2, NR_MKDIRAT, NR_MKNODAT] {
            a.jump(JEQ_K, nr, Some(mode_2), None);
        }
        a.jump(JEQ_K, NR_OPEN, Some(open), None);
        a.jump(JEQ_K, NR_OPENAT, Some(openat), None);
        for nr in [
            NR_OPENAT2,
            NR_IO_URING_SETUP,
            NR_IO_URING_ENTER,
            NR_IO_URING_REGISTER,
        ] {
            a.jump(JEQ_K, nr, Some(nosys), None);
        }
        a.stmt(RET_K, libc::SECCOMP_RET_ALLOW);

        // open(path, flags = argument 1, mode = argument 2)
        a.here(open);
        a.stmt(LD_W_ABS, arg(1));
        a.jump(JSET_K, CREATING, Some(mode_2), Some(allow));

        // openat(dfd, path, flags = argument 2, mode = argument 3)
        a.here(openat);
        a.stmt(LD_W_ABS, arg(2));
        let mode_3 = a.label();
        a.jump(JSET_K, CREATING, Some(mode_3), Some(allow));
        a.here(mode_3);
        a.stmt(LD_W_ABS, arg(3));
        a.jump(JSET_K, SET_ID, Some(deny), Some(allow));

        a.here(mode_1);
        a.stmt(LD_W_ABS, arg(1));
        a.jump(JSET_K, SET_ID, Some(deny), Some(allow));

        a.here(mode_2);
        a.stmt(LD_W_ABS, arg(2));
        a.jump(JSET_K, SET_ID, Some(deny), Some(allow));

        a.here(allow);
        a.stmt(RET_K, libc::SECCOMP_RET_ALLOW);
        a.here(deny);
        a.stmt(
            RET_K,
            libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA),
        );
        a.here(nosys);
        a.stmt(
            RET_K,
            libc::SECCOMP_RET_ERRNO | (libc::ENOSYS as u32 & libc::SECCOMP_RET_DATA),
        );
        a.here(kill);
        a.stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS);

        Filter {
            program: a.finish(),
        }
    }

    /// No-new-privileges, then the filter, for this process and everything
    /// it starts. Only system calls: safe between `fork` and `exec`.
    pub fn install(&self) -> io::Result<()> {
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let program = sock_fprog {
                len: self.program.len() as u16,
                filter: self.program.as_ptr() as *mut sock_filter,
            };
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &program as *const sock_fprog as libc::c_ulong,
                0,
                0,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
