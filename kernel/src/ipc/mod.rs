//! IPC — Inter-Process Communication
//!
//! Lock-free SPSC ring buffers, wrapped in a default-deny channel registry.
//! Every channel has an owner (uid) and an explicit allow-list of uids
//! permitted to send/recv on it - no open bus any process can shout on,
//! per the Qubes qrexec-derived design principle (Part 3.5 of the roadmap).
//! Uses the same caller-identity accessor as VFS (fs::vfs::current_uid(),
//! itself backed by real per-Tcb state via the scheduler - Part 6 item 2)
//! rather than a second, divergent identity mechanism.

extern crate alloc;
use alloc::{boxed::Box, string::String, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::RwLock;

// ─── Lock-free SPSC ring ──────────────────────────────────────────────────────

const RING_SIZE: usize = 4096;  // must be power of 2

pub struct Ring {
    buf:  Box<[u8; RING_SIZE]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl Ring {
    pub fn new() -> Self {
        Self {
            buf:  Box::new([0u8; RING_SIZE]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, data: &[u8]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = RING_SIZE - 1 - (head.wrapping_sub(tail) & (RING_SIZE - 1));
        let n    = data.len().min(free);
        for (i, &b) in data[..n].iter().enumerate() {
            let idx = head.wrapping_add(i) & (RING_SIZE - 1);
            unsafe { (self.buf.as_ptr() as *mut u8).add(idx).write(b); }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    pub fn pop(&self, buf: &mut [u8]) -> usize {
        let tail  = self.tail.load(Ordering::Relaxed);
        let head  = self.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail) & (RING_SIZE - 1);
        let n     = buf.len().min(avail);
        for i in 0..n {
            let idx = tail.wrapping_add(i) & (RING_SIZE - 1);
            unsafe { buf[i] = self.buf.as_ptr().add(idx).read(); }
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    pub fn available(&self) -> usize {
        let h = self.head.load(Ordering::Acquire);
        let t = self.tail.load(Ordering::Acquire);
        h.wrapping_sub(t) & (RING_SIZE - 1)
    }
}

// ─── Channel registry (default-deny) ──────────────────────────────────────────

pub type ChannelId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    NotFound,
    PermissionDenied,
}

struct Channel {
    id:      ChannelId,
    name:    String,
    owner:   u32,       // uid that created the channel
    allowed: Vec<u32>,  // uids explicitly granted send/recv - owner is always implicitly included
    ring:    Ring,
}

static CHANNELS: RwLock<Vec<Channel>> = RwLock::new(Vec::new());
static NEXT_ID:  AtomicUsize = AtomicUsize::new(1);

fn current_uid() -> u32 {
    crate::fs::vfs::current_uid()
}

pub fn init() {
    crate::kprintln!("[ipc] Initialised");
}

/// Create a channel. The caller (current_uid()) becomes its owner and is
/// implicitly allowed to send/recv - no one else is, until granted.
pub fn create_channel(name: &str) -> ChannelId {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as ChannelId;
    let owner = current_uid();
    CHANNELS.write().push(Channel {
        id,
        name: String::from(name),
        owner,
        allowed: Vec::new(),
        ring: Ring::new(),
    });
    crate::kprintln!("[ipc] Channel '{}' (id={}) created, owner uid={}", name, id, owner);
    id
}

/// Owner-only: grant another uid permission to send/recv on this channel.
/// Returns PermissionDenied if the caller isn't the owner, NotFound if the
/// channel doesn't exist.
pub fn grant(id: ChannelId, uid: u32) -> Result<(), IpcError> {
    let caller = current_uid();
    let mut channels = CHANNELS.write();
    let ch = match channels.iter_mut().find(|c| c.id == id) {
        Some(c) => c,
        None => {
            crate::audit::audit_log("ipc", "grant", caller, false, alloc::format!("channel {} not found", id));
            return Err(IpcError::NotFound);
        }
    };
    if ch.owner != caller {
        crate::audit::audit_log("ipc", "grant", caller, false, alloc::format!("channel {} (not owner)", id));
        return Err(IpcError::PermissionDenied);
    }
    if !ch.allowed.contains(&uid) {
        ch.allowed.push(uid);
    }
    crate::audit::audit_log("ipc", "grant", caller, true, alloc::format!("channel {} -> uid {}", id, uid));
    Ok(())
}

fn can_access(ch: &Channel, uid: u32) -> bool {
    ch.owner == uid || ch.allowed.contains(&uid)
}

/// Send on a channel. Denied unless the caller is the owner or has been
/// explicitly granted access via grant() - no implicit access.
pub fn send(id: ChannelId, data: &[u8]) -> Result<usize, IpcError> {
    let caller = current_uid();
    let channels = CHANNELS.read();
    let ch = match channels.iter().find(|c| c.id == id) {
        Some(c) => c,
        None => {
            crate::audit::audit_log("ipc", "send", caller, false, alloc::format!("channel {} not found", id));
            return Err(IpcError::NotFound);
        }
    };
    if !can_access(ch, caller) {
        crate::audit::audit_log("ipc", "send", caller, false, alloc::format!("channel {}", id));
        return Err(IpcError::PermissionDenied);
    }
    crate::audit::audit_log("ipc", "send", caller, true, alloc::format!("channel {}", id));
    Ok(ch.ring.push(data))
}

/// Receive from a channel. Same default-deny check as send().
pub fn recv(id: ChannelId, buf: &mut [u8]) -> Result<usize, IpcError> {
    let caller = current_uid();
    let channels = CHANNELS.read();
    let ch = match channels.iter().find(|c| c.id == id) {
        Some(c) => c,
        None => {
            crate::audit::audit_log("ipc", "recv", caller, false, alloc::format!("channel {} not found", id));
            return Err(IpcError::NotFound);
        }
    };
    if !can_access(ch, caller) {
        crate::audit::audit_log("ipc", "recv", caller, false, alloc::format!("channel {}", id));
        return Err(IpcError::PermissionDenied);
    }
    crate::audit::audit_log("ipc", "recv", caller, true, alloc::format!("channel {}", id));
    Ok(ch.ring.pop(buf))
}

// ─── Compile-compatibility stubs only ─────────────────────────────────────────
//
// The AI protocol (AiReqPacket/AiRespPacket/get_ai_completion/AI_REQ_CHANNEL)
// that used to live here has been removed - it was scope creep ahead of the
// security foundation, per Part 3.1/Part 1's own stated rule that the AI
// track is deliberately set aside. It is NOT being rebuilt here.
//
// These stubs exist ONLY because other still-compiled, still-unwired modules
// (fault/, heal/, the separate kshell/mod.rs AI shell, syscall/mod.rs) call
// these names. None of that code runs on the live kernel_main_native boot
// path. Do not build real AI behavior into these - if that work ever
// happens, it happens deliberately, later, per the roadmap.

pub fn spawn_userspace(name: &str) {
    crate::kprintln!("[ipc] spawn_userspace stub called for '{}' (inert)", name);
}

pub static AI_REQ_CHANNEL: spin::Once<ChannelId> = spin::Once::new();

pub fn get_ai_completion(_prefix: &str, _lang: &str, _max_tokens: u16) -> String {
    String::new()
}
