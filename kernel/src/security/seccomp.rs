//! Seccomp — System Call Filter
//!
//! Every process has an associated seccomp profile that specifies
//! which syscalls are ALLOWED. Any other syscall terminates the process.
//!
//! This prevents exploited processes from using dangerous syscalls
//! (e.g. `execve`, `mmap`, `ptrace`) even if they gain code execution.
//!
//! Profile types:
//!   Unconfined  — all syscalls allowed (kernel/init only)
//!   Default     — common safe syscalls (suitable for most apps)
//!   Strict      — read/write/exit/sigreturn only (maximum lockdown)
//!   Custom(set) — caller-defined whitelist

extern crate alloc;
use alloc::vec::Vec;
use spin::RwLock;

// ─── Syscall numbers (x86_64 Linux ABI compatible) ────────────────────────────

/// We use Linux-compatible syscall numbers so existing ELF binaries work.
#[allow(dead_code)]
pub mod nr {
    pub const READ:          u32 = 0;
    pub const WRITE:         u32 = 1;
    pub const OPEN:          u32 = 2;
    pub const CLOSE:         u32 = 3;
    pub const STAT:          u32 = 4;
    pub const FSTAT:         u32 = 5;
    pub const LSTAT:         u32 = 6;
    pub const POLL:          u32 = 7;
    pub const LSEEK:         u32 = 8;
    pub const MMAP:          u32 = 9;
    pub const MPROTECT:      u32 = 10;
    pub const MUNMAP:        u32 = 11;
    pub const BRK:           u32 = 12;
    pub const RT_SIGACTION:  u32 = 13;
    pub const RT_SIGRETURN:  u32 = 15;
    pub const IOCTL:         u32 = 16;
    pub const READV:         u32 = 19;
    pub const WRITEV:        u32 = 20;
    pub const EXIT:          u32 = 60;
    pub const EXIT_GROUP:    u32 = 231;
    pub const GETPID:        u32 = 39;
    pub const GETPPID:       u32 = 110;
    pub const FORK:          u32 = 57;
    pub const CLONE:         u32 = 56;
    pub const EXECVE:        u32 = 59;
    pub const KILL:          u32 = 62;
    pub const PTRACE:        u32 = 101;
    pub const SOCKET:        u32 = 41;
    pub const CONNECT:       u32 = 42;
    pub const ACCEPT:        u32 = 43;
    pub const SENDTO:        u32 = 44;
    pub const RECVFROM:      u32 = 45;
    pub const BIND:          u32 = 49;
    pub const LISTEN:        u32 = 50;
    pub const GETUID:        u32 = 102;
    pub const GETGID:        u32 = 104;
    pub const NANOSLEEP:     u32 = 35;
    pub const CLOCK_GETTIME: u32 = 228;
    pub const FUTEX:         u32 = 202;
    pub const OPENAT:        u32 = 257;
    pub const NEWFSTATAT:    u32 = 262;
    pub const READLINKAT:    u32 = 267;
    pub const GETDENTS64:    u32 = 217;
    pub const FCNTL:         u32 = 72;
    pub const PIPE2:         u32 = 293;
    pub const DUP2:          u32 = 33;
    pub const EPOLL_CREATE1: u32 = 291;
    pub const EPOLL_CTL:     u32 = 233;
    pub const EPOLL_WAIT:    u32 = 232;
    pub const SENDFILE:      u32 = 40;
    pub const ACCEPT4:       u32 = 288;
    pub const PRCTL:         u32 = 157;
    pub const SECCOMP:       u32 = 317;
    pub const GETRANDOM:     u32 = 318;
}

// ─── Profile ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SeccompProfile {
    /// No restrictions — kernel processes only.
    Unconfined,
    /// Read, write, exit, sigreturn — bare minimum for compute workloads.
    Strict,
    /// Default set suitable for a general application.
    Default,
    /// Networking application preset (includes socket/connect/send/recv).
    Network,
    /// Custom whitelist.
    Allow(Vec<u32>),
}

impl SeccompProfile {
    /// Check if a syscall number is permitted.
    pub fn allows(&self, sysno: u32) -> bool {
        match self {
            Self::Unconfined => true,

            Self::Strict => matches!(sysno,
                nr::READ | nr::WRITE | nr::EXIT | nr::EXIT_GROUP |
                nr::RT_SIGRETURN | nr::NANOSLEEP
            ),

            Self::Default => matches!(sysno,
                nr::READ        | nr::WRITE       | nr::OPEN      |
                nr::OPENAT      | nr::CLOSE       | nr::STAT      |
                nr::FSTAT       | nr::LSTAT       | nr::NEWFSTATAT |
                nr::LSEEK       | nr::MMAP        | nr::MPROTECT  |
                nr::MUNMAP      | nr::BRK         | nr::RT_SIGACTION |
                nr::RT_SIGRETURN| nr::IOCTL       | nr::READV     |
                nr::WRITEV      | nr::GETDENTS64  | nr::READLINKAT |
                nr::FCNTL       | nr::PIPE2       | nr::DUP2      |
                nr::POLL        | nr::EPOLL_CREATE1 | nr::EPOLL_CTL |
                nr::EPOLL_WAIT  | nr::SENDFILE    | nr::FUTEX     |
                nr::NANOSLEEP   | nr::CLOCK_GETTIME | nr::GETUID  |
                nr::GETGID      | nr::GETPID      | nr::GETPPID   |
                nr::EXIT        | nr::EXIT_GROUP  | nr::PRCTL     |
                nr::GETRANDOM
            ),

            Self::Network => {
                // Default + socket syscalls
                SeccompProfile::Default.allows(sysno) || matches!(sysno,
                    nr::SOCKET  | nr::CONNECT | nr::ACCEPT  |
                    nr::ACCEPT4 | nr::SENDTO  | nr::RECVFROM |
                    nr::BIND    | nr::LISTEN
                )
            }

            Self::Allow(list) => list.contains(&sysno),
        }
    }

    /// What to do on a denied syscall.
    /// Returns `SeccompAction::Kill` always — denial is fatal.
    pub fn on_deny(&self) -> SeccompAction {
        SeccompAction::Kill
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompAction {
    /// Terminate process with SIGSYS.
    Kill,
    /// Return EPERM to caller.
    Errno,
    /// Log and allow (audit mode — useful for policy development).
    Log,
}

// ─── Per-process seccomp state ────────────────────────────────────────────────

static PROFILES: RwLock<Vec<(u32 /*pid*/, SeccompProfile)>> = RwLock::new(Vec::new());

/// Initialise the seccomp subsystem. PID 0 is unconfined.
pub fn init() {
    PROFILES.write().push((0, SeccompProfile::Unconfined));
    crate::kprintln!("[seccomp] Syscall filter initialised");
}

/// Set a profile for a process.
pub fn set_profile(pid: u32, profile: SeccompProfile) {
    let mut profiles = PROFILES.write();
    if let Some(entry) = profiles.iter_mut().find(|(p, _)| *p == pid) {
        entry.1 = profile;
    } else {
        profiles.push((pid, profile));
    }
}

/// Called from the syscall dispatcher. Returns what should happen.
/// This is called on every single syscall — must be fast.
///
/// Time: O(n) for Custom profile (n = whitelist len), O(1) for others.
#[inline]
pub fn check_syscall(pid: u32, sysno: u32) -> SeccompAction {
    let profiles = PROFILES.read();
    match profiles.iter().find(|(p, _)| *p == pid) {
        Some((_, profile)) => {
            if profile.allows(sysno) {
                SeccompAction::Log // = allow (used for audit)
            } else {
                profile.on_deny()
            }
        }
        // Unknown PID → deny by default
        None => SeccompAction::Kill,
    }
}

/// Remove a profile on process exit.
pub fn deregister(pid: u32) {
    PROFILES.write().retain(|(p, _)| *p != pid);
}
