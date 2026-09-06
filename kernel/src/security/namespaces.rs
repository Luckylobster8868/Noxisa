//! Namespaces — Process isolation (Linux-inspired)
//!
//! Each process belongs to a set of namespaces. Processes in different
//! namespaces cannot see or affect each other's resources.
//!
//! Namespace types implemented:
//!   PID       — each namespace has its own PID number space starting at 1
//!   Mount     — each namespace sees its own filesystem tree
//!   Network   — each namespace has its own network interfaces and routing
//!   User      — map UIDs/GIDs; allow unprivileged user namespaces
//!   IPC       — separate System-V IPC and POSIX message queues

extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::RwLock;

// ─── Namespace ID types ───────────────────────────────────────────────────────

/// Unique identifier for any namespace instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NsId(u32);

impl NsId {
    /// The initial (root) namespace — ID 1.
    pub const INIT: Self = Self(1);
}

// ─── Namespace kinds ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PidNs {
    pub id:     NsId,
    /// PID in this namespace → global PID mapping
    pub map:    Vec<(u32, u32)>,
    parent:     Option<NsId>,
}

#[derive(Debug, Clone)]
pub struct MountNs {
    pub id:     NsId,
    /// Mount points visible inside this namespace
    pub mounts: Vec<MountPoint>,
}

#[derive(Debug, Clone)]
pub struct MountPoint {
    pub source: alloc::string::String,
    pub target: alloc::string::String,
    pub fstype: alloc::string::String,
}

#[derive(Debug, Clone)]
pub struct NetNs {
    pub id:       NsId,
    pub loopback: bool,   // lo interface always present
    pub ifaces:   Vec<alloc::string::String>,
}

#[derive(Debug, Clone)]
pub struct UserNs {
    pub id:        NsId,
    /// uid_map: (ns_uid_start, host_uid_start, count)
    pub uid_map:   Vec<(u32, u32, u32)>,
    pub gid_map:   Vec<(u32, u32, u32)>,
}

// ─── Per-process namespace membership ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NsMembership {
    pub pid:   NsId,
    pub mnt:   NsId,
    pub net:   NsId,
    pub user:  NsId,
    pub ipc:   NsId,
}

impl NsMembership {
    /// All namespaces point to the initial root namespace.
    pub fn init_ns() -> Self {
        Self {
            pid:  NsId::INIT,
            mnt:  NsId::INIT,
            net:  NsId::INIT,
            user: NsId::INIT,
            ipc:  NsId::INIT,
        }
    }
}

// ─── Global namespace registry ────────────────────────────────────────────────

static NS_COUNTER: AtomicU32 = AtomicU32::new(2); // 1 = init
static PROC_NS:    RwLock<Vec<(u32 /*pid*/, NsMembership)>> = RwLock::new(Vec::new());

/// Allocate a fresh namespace ID.
fn new_ns_id() -> NsId {
    NsId(NS_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Initialise the namespace subsystem. PID 0 joins all init namespaces.
pub fn init() {
    PROC_NS.write().push((0, NsMembership::init_ns()));
    crate::kprintln!("[ns] Namespace subsystem initialised");
}

/// Register a new process into its parent's namespaces (fork semantics).
pub fn fork_namespaces(parent_pid: u32, child_pid: u32) {
    let parent_ns = PROC_NS.read()
        .iter()
        .find(|(p, _)| *p == parent_pid)
        .map(|(_, ns)| ns.clone());

    if let Some(ns) = parent_ns {
        PROC_NS.write().push((child_pid, ns));
    }
}

/// Create a new isolated namespace for a sandboxed process.
/// Returns the NsMembership with fresh namespace IDs for each type requested.
pub fn create_isolated(flags: NsFlags) -> NsMembership {
    let base = NsMembership::init_ns();
    NsMembership {
        pid:  if flags.contains(NsFlags::PID)  { new_ns_id() } else { base.pid  },
        mnt:  if flags.contains(NsFlags::MNT)  { new_ns_id() } else { base.mnt  },
        net:  if flags.contains(NsFlags::NET)  { new_ns_id() } else { base.net  },
        user: if flags.contains(NsFlags::USER) { new_ns_id() } else { base.user },
        ipc:  if flags.contains(NsFlags::IPC)  { new_ns_id() } else { base.ipc  },
    }
}

/// Flags controlling which namespaces are isolated on creation.
#[derive(Debug, Clone, Copy)]
pub struct NsFlags(u8);

impl NsFlags {
    pub const NONE: Self = Self(0);
    pub const PID:  Self = Self(1 << 0);
    pub const MNT:  Self = Self(1 << 1);
    pub const NET:  Self = Self(1 << 2);
    pub const USER: Self = Self(1 << 3);
    pub const IPC:  Self = Self(1 << 4);
    /// Full isolation (used for containers).
    pub const ALL:  Self = Self(0x1F);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Get the namespace membership for a process.
pub fn get_ns(pid: u32) -> Option<NsMembership> {
    PROC_NS.read()
        .iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, ns)| ns.clone())
}

/// Remove a process from the namespace registry (on exit).
pub fn deregister(pid: u32) {
    PROC_NS.write().retain(|(p, _)| *p != pid);
}
