//! Kernel-level audit logging.
//!
//! A small, standalone log - deliberately NOT part of the unwired,
//! scaffolded six-layer `security/` module (capabilities/namespaces/seccomp/
//! MAC/memory_hardening/hardware), which is never actually called from
//! kernel_main_native and would repeat the same "structure ahead of
//! foundation" mistake already rejected once for ipc's AI protocol.
//!
//! Instead, this hooks into the chokepoints that already make real allow/deny
//! decisions: checked_sys_write (capabilities), vfs::read/write (VFS
//! permissions), ipc::send/recv/grant (IPC). Every one of those calls
//! audit_log() once, on both the allow and the deny path, so "what did this
//! process actually try to do" is answerable even for denied attempts -
//! which is the actual point, per the roadmap's own reference table: this
//! must be answerable even if the process/shell issuing it is compromised.

extern crate alloc;
use alloc::string::String;
use spin::Mutex;

const LOG_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct AuditEvent {
    pub tick:      u64,
    pub subsystem: &'static str,  // "cap", "vfs", "ipc"
    pub action:    &'static str,  // "write", "read", "send", "recv", "grant"
    pub uid:       u32,
    pub allowed:   bool,
    pub detail:    String,        // e.g. path, channel id, capability name
}

struct AuditLog {
    events: alloc::collections::VecDeque<AuditEvent>,
}

static LOG: Mutex<AuditLog> = Mutex::new(AuditLog { events: alloc::collections::VecDeque::new() });

/// Record one audit event. Called from every real allow/deny chokepoint,
/// on both outcomes - a denial is at least as important to record as an
/// allow, since "what did this process try to do" matters most when the
/// answer is "something it wasn't allowed to."
pub fn audit_log(subsystem: &'static str, action: &'static str, uid: u32, allowed: bool, detail: alloc::string::String) {
    let event = AuditEvent {
        tick: crate::drivers::pit::ticks(),
        subsystem,
        action,
        uid,
        allowed,
        detail,
    };
    let mut log = LOG.lock();
    if log.events.len() >= LOG_CAPACITY {
        log.events.pop_front();  // ring buffer: drop oldest, never block/panic on a full log
    }
    log.events.push_back(event);
}

/// Return a snapshot copy of all currently-held audit events, oldest first.
pub fn snapshot() -> alloc::vec::Vec<AuditEvent> {
    LOG.lock().events.iter().cloned().collect()
}

pub fn count() -> usize {
    LOG.lock().events.len()
}
