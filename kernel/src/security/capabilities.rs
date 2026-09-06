//! Capabilities — Fine-grained privilege tokens
//!
//! Replaces the binary root/non-root model with 64 individual capabilities.
//! Inspired by Linux capabilities but with cleaner semantics.
//!
//! Every process has three capability sets:
//!   Permitted   — maximum capabilities the process MAY hold
//!   Effective   — capabilities currently active (used for checks)
//!   Inheritable — capabilities passed to child processes on exec
//!
//! Key properties:
//!   • A process can only GRANT capabilities it already has in its Permitted set.
//!   • Dropping a capability is permanent (can never be re-acquired).
//!   • Kernel init (PID 0) starts with ALL capabilities.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::RwLock;
use alloc::vec::Vec;

extern crate alloc;

// ─── Capability definitions ───────────────────────────────────────────────────

/// Each bit represents one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapSet(pub u64);

impl CapSet {
    pub const EMPTY: Self = Self(0);
    pub const ALL:   Self = Self(u64::MAX);

    /// Test if a capability is present.
    #[inline]
    pub fn has(self, cap: Cap) -> bool {
        self.0 & (1u64 << cap as u32) != 0
    }

    /// Return the union of two capability sets.
    #[inline]
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Return the intersection.
    #[inline]
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Remove a capability. This operation is irreversible.
    #[inline]
    pub fn drop(self, cap: Cap) -> Self {
        Self(self.0 & !(1u64 << cap as u32))
    }
}

/// Individual capability constants.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// Change process UID/GID (setuid/setgid).
    SetUid           = 0,
    /// Bypass filesystem permission checks (like root).
    DacOverride      = 1,
    /// Read filesystem DAC override.
    DacReadSearch    = 2,
    /// Set file ownership (chown).
    Chown            = 3,
    /// Bind to privileged ports (< 1024).
    NetBindService   = 4,
    /// Broadcast/multicast networking.
    NetBroadcast     = 5,
    /// Raw socket access (ping, packet capture).
    NetRaw           = 6,
    /// Network admin (interface config, routing).
    NetAdmin         = 7,
    /// System reboot / kexec.
    SysReboot        = 8,
    /// Mount / unmount filesystems.
    SysMount         = 9,
    /// Load kernel modules.
    SysModule        = 10,
    /// Use ptrace on any process.
    SysPtrace        = 11,
    /// Process scheduling (nice, setpriority).
    SysNice          = 12,
    /// Override resource limits.
    SysResource      = 13,
    /// Set system time.
    SysTime          = 14,
    /// Manage IPC objects.
    IpcOwner         = 15,
    /// Kill any process (even in other PID namespaces).
    Kill             = 16,
    /// Read audit log.
    AuditRead        = 17,
    /// Write audit log.
    AuditWrite       = 18,
    /// Configure security policy (MAC label changes).
    MacAdmin         = 19,
    /// Override MAC policy (emergency break-glass).
    MacOverride      = 20,
    /// Access hardware devices directly.
    SysRawIo         = 21,
    /// Use chroot syscall.
    SysChroot        = 22,
    /// Lock memory (mlock/mlockall).
    IpcLock          = 23,
    /// Create new user namespaces.
    SetpcapUser      = 24,
    // 25–63 reserved for future use
}

// ─── Per-process capability state ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProcCaps {
    pub permitted:   CapSet,
    pub effective:   CapSet,
    pub inheritable: CapSet,
    /// Ambient capabilities — auto-granted to unprivileged exec'd processes.
    pub ambient:     CapSet,
}

impl ProcCaps {
    /// Full privileges: kernel init (PID 0).
    pub fn all_powerful() -> Self {
        Self {
            permitted:   CapSet::ALL,
            effective:   CapSet::ALL,
            inheritable: CapSet::EMPTY,
            ambient:     CapSet::EMPTY,
        }
    }

    /// No special privileges: a freshly created user process.
    pub fn unprivileged() -> Self {
        Self {
            permitted:   CapSet::EMPTY,
            effective:   CapSet::EMPTY,
            inheritable: CapSet::EMPTY,
            ambient:     CapSet::EMPTY,
        }
    }

    /// Daemon preset — common set for system daemons.
    /// Has network-bind and resource-limit but NOT raw I/O or ptrace.
    pub fn daemon() -> Self {
        let set = CapSet(
            (1 << Cap::NetBindService as u32) |
            (1 << Cap::NetBroadcast   as u32) |
            (1 << Cap::SysResource    as u32) |
            (1 << Cap::IpcOwner       as u32) |
            (1 << Cap::AuditWrite     as u32)
        );
        Self {
            permitted:   set,
            effective:   set,
            inheritable: CapSet::EMPTY,
            ambient:     CapSet::EMPTY,
        }
    }

    /// Check if the process has an effective capability.
    #[inline]
    pub fn check(&self, cap: Cap) -> bool {
        self.effective.has(cap)
    }

    /// Drop a capability from all sets permanently.
    pub fn drop(&mut self, cap: Cap) {
        self.permitted   = self.permitted.drop(cap);
        self.effective   = self.effective.drop(cap);
        self.inheritable = self.inheritable.drop(cap);
        self.ambient     = self.ambient.drop(cap);
    }

    /// Compute child capabilities on fork/exec.
    /// Child inheritable = parent.inheritable ∩ allowed_inheritable
    pub fn exec_child(&self, file_inheritable: CapSet) -> Self {
        let inheritable = self.inheritable.intersect(file_inheritable);
        let permitted   = inheritable.union(self.ambient);
        Self {
            permitted,
            effective: permitted,   // file can set effective = permitted
            inheritable,
            ambient: self.ambient,
        }
    }
}

// ─── Global capability table ──────────────────────────────────────────────────

static CAP_TABLE: RwLock<Vec<(u32 /*pid*/, ProcCaps)>> = RwLock::new(Vec::new());

/// Initialise the capability subsystem. Grants kernel PID 0 all capabilities.
pub fn init() {
    CAP_TABLE.write().push((0, ProcCaps::all_powerful()));
    crate::kprintln!("[caps] Capability system initialised");
}

/// Register a new process's capabilities.
pub fn register(pid: u32, caps: ProcCaps) {
    let mut table = CAP_TABLE.write();
    if let Some(entry) = table.iter_mut().find(|(p, _)| *p == pid) {
        entry.1 = caps;
    } else {
        table.push((pid, caps));
    }
}

/// Check if `pid` has an effective capability. O(n) scan, n = live processes.
pub fn check(pid: u32, cap: Cap) -> bool {
    CAP_TABLE.read()
        .iter()
        .find(|(p, _)| *p == pid)
        .map_or(false, |(_, caps)| caps.check(cap))
}

/// Remove a process from the capability table (on exit).
pub fn deregister(pid: u32) {
    CAP_TABLE.write().retain(|(p, _)| *p != pid);
}
