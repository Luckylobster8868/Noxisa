//! Noxisa Security Subsystem
//!
//! Five interlocking layers — every process passes through ALL of them:
//!
//!  ┌──────────────────────────────────────────────────────────┐
//!  │  Layer 1 — Capabilities                                  │
//!  │  Fine-grained privileges replacing all-or-nothing root.  │
//!  │  Each process starts with the minimum set it needs.      │
//!  ├──────────────────────────────────────────────────────────┤
//!  │  Layer 2 — Namespaces                                    │
//!  │  PID / Mount / Network / User / IPC isolation.           │
//!  │  Process can only see what's in its namespace.           │
//!  ├──────────────────────────────────────────────────────────┤
//!  │  Layer 3 — Seccomp (syscall filter)                      │
//!  │  Each process has a whitelist of allowed syscalls.       │
//!  │  Attempting a blocked syscall → SIGKILL (not EPERM).     │
//!  ├──────────────────────────────────────────────────────────┤
//!  │  Layer 4 — MAC (Mandatory Access Control)                │
//!  │  Every resource has a label; policy rules determine      │
//!  │  which labels can read/write/execute which resources.    │
//!  │  Inspired by SELinux type enforcement.                   │
//!  ├──────────────────────────────────────────────────────────┤
//!  │  Layer 5 — Memory hardening                              │
//!  │  ASLR + stack canaries + NX pages + guard pages.         │
//!  └──────────────────────────────────────────────────────────┘

pub mod capabilities;
pub mod hardware;
pub mod mac;
pub mod memory_hardening;
pub mod namespaces;
pub mod seccomp;

/// Initialise the full security subsystem. Called once from kernel_main.
///
/// Layer order matters:
///  1. hardware  — SMEP/SMAP/NX/PKS must be on before anything runs in user-space
///  2. memory_hardening — ASLR seed needs RDRAND (hardware already probed)
///  3. capabilities — before any process is created
///  4. namespaces  — before any fork
///  5. seccomp     — profile for PID 0
///  6. mac         — policy loaded last (depends on labels from caps/ns)
pub fn init() {
    hardware::init();          // SMEP, SMAP, NX, PKS (BULKHEAD-style)
    memory_hardening::init();  // ASLR, canaries, guard pages
    capabilities::init();      // capability table, PID 0 = all_powerful
    namespaces::init();        // PID/MNT/NET/USER/IPC namespaces
    seccomp::init();           // syscall filter, PID 0 = unconfined
    mac::init();               // mandatory access control, default policy
    crate::kprintln!("[security] All 6 layers active (HW+ASLR+Caps+NS+Seccomp+MAC)");
}
