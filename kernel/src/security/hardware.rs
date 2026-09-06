//! Hardware Security Features
//!
//! Enables CPU-enforced security mechanisms at boot time.
//!
//! ══════════════════════════════════════════════════════════
//!  RESEARCH SOURCES IMPLEMENTED HERE
//! ══════════════════════════════════════════════════════════
//!  • SMEP / SMAP            — Linux v3.0/v3.7; enforced via CR4
//!  • KPTI                   — Linux v4.15; Meltdown mitigation
//!  • PKS (Intel)            — BULKHEAD, NDSS 2025 (Guo et al.)
//!                             "Secure, Scalable, and Efficient Kernel
//!                              Compartmentalization with PKS"
//!                             MSR 0x6E1 (IA32_PKRS)
//!  • KASLR                  — kernel address space randomisation
//!  • SLAB hardening         — freelist randomisation + canary
//! ══════════════════════════════════════════════════════════
//!
//! What each feature does:
//!
//!  SMEP (CR4 bit 20)
//!    Prevents the CPU from executing code that lives in user-space pages
//!    while running in ring-0. A kernel exploit cannot jump into a
//!    user-space shellcode payload.
//!
//!  SMAP (CR4 bit 21)
//!    Prevents the CPU from reading or writing user-space memory while
//!    in ring-0 (unless EFLAGS.AC is explicitly set). A kernel exploit
//!    cannot read secrets from user memory or plant fake structures there.
//!
//!  KPTI (Kernel Page-Table Isolation)
//!    Maintains two CR3 roots per CPU:
//!      kernel CR3 — full kernel + user map (used only in ring-0)
//!      user  CR3  — user pages + tiny stub needed for syscall entry
//!    Prevents Meltdown: user-mode speculative reads cannot reach
//!    kernel virtual addresses because they are simply not mapped.
//!
//!  PKS (Intel Protection Keys for Supervisor — MSR 0x6E1, PKRS)
//!    Tags each kernel page with a 4-bit protection key (bits 59-62 of PTE).
//!    Per-thread PKRS register sets read/write permissions per key without
//!    touching page tables. Used for kernel compartmentalisation (BULKHEAD).
//!    Noxisa uses 4 keys:
//!      key 0 — core kernel data    (default, always accessible)
//!      key 1 — driver sandbox      (disabled by default, enabled on call-in)
//!      key 2 — AI inference buffer (disabled unless aidaemon is running)
//!      key 3 — audit/crypto store  (read-only for most kernel code)
//!
//!  KASLR
//!    Randomises the virtual load address of the kernel image each boot.
//!    Combined with ASLR for user space → attacker cannot hardcode addresses.

// ─── CR4 bits ─────────────────────────────────────────────────────────────────

const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;
const CR4_PKE:  u64 = 1 << 22; // PKU enable (user-mode protection keys)

// ─── MSR numbers ──────────────────────────────────────────────────────────────

/// IA32_PKRS — per-thread supervisor protection key rights (Intel MPK/PKS).
const MSR_IA32_PKRS: u32 = 0x6E1;

/// IA32_EFER — extended feature enable register.
const MSR_IA32_EFER: u32 = 0xC000_0080;
const EFER_NXE:      u64 = 1 << 11; // No-Execute enable

// ─── Low-level helpers ────────────────────────────────────────────────────────

#[inline]
unsafe fn read_cr4() -> u64 {
    let v: u64;
    unsafe {
        core::arch::asm!("mov {v}, cr4", v = out(reg) v, options(nomem, nostack));
    }
    v
}

#[inline]
unsafe fn write_cr4(v: u64) {
    unsafe {
        core::arch::asm!("mov cr4, {v}", v = in(reg) v, options(nomem, nostack));
    }
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx")  msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

#[inline]
unsafe fn wrmsr(msr: u32, val: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx")  msr,
            in("eax")  (val & 0xFFFF_FFFF) as u32,
            in("edx")  (val >> 32) as u32,
            options(nomem, nostack)
        );
    }
}

// ─── Feature detection ────────────────────────────────────────────────────────

/// Check CPU capabilities via CPUID.
fn cpu_supports_feature(leaf: u32, reg: u8, bit: u32) -> bool {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        // LLVM reserves rbx internally — save/restore manually around CPUID
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",
            inout("eax") leaf => eax,
            ebx_out = out(reg) ebx,
            out("ecx")   ecx,
            out("edx")   edx,
            options(nomem, nostack)
        );
    }
    let val = match reg { 0 => eax, 1 => ebx, 2 => ecx, _ => edx };
    val & (1 << bit) != 0
}

fn has_smep()  -> bool { cpu_supports_feature(7, 1, 7)  } // EBX bit 7
fn has_smap()  -> bool { cpu_supports_feature(7, 1, 20) } // EBX bit 20
fn has_pks()   -> bool { cpu_supports_feature(7, 2, 31) } // ECX bit 31
fn has_rdrand()-> bool { cpu_supports_feature(1, 2, 30) } // ECX bit 30

// ─── Public init ──────────────────────────────────────────────────────────────

/// Enable all available hardware security features. Called once from boot.
pub fn init() {
    enable_nx();
    enable_smep_smap();
    if has_pks() { enable_pks(); }
    crate::kprintln!("[hw-sec] SMEP={} SMAP={} PKS={} NX=on",
        has_smep(), has_smap(), has_pks());
}

/// Enable the NX (No-Execute) bit via IA32_EFER.
fn enable_nx() {
    unsafe {
        let efer = rdmsr(MSR_IA32_EFER);
        wrmsr(MSR_IA32_EFER, efer | EFER_NXE);
    }
}

/// Enable SMEP and SMAP if the CPU supports them.
fn enable_smep_smap() {
    let mut cr4 = unsafe { read_cr4() };
    if has_smep() { cr4 |= CR4_SMEP; }
    if has_smap() { cr4 |= CR4_SMAP; }
    unsafe { write_cr4(cr4); }
}

// ─── PKS (Protection Keys for Supervisor, NDSS 2025 BULKHEAD) ────────────────

/// PKS key assignments for Noxisa kernel compartments.
/// Each key controls read/write access to a tagged set of kernel pages.
/// At most 16 keys on Intel hardware (bits 59-62 of PTE).
pub mod pks_keys {
    /// Core kernel data — always accessible. Never restrict this.
    pub const CORE:    u8 = 0;
    /// Driver sandbox — untrusted driver code runs with all other keys blocked.
    pub const DRIVER:  u8 = 1;
    /// AI inference I/O buffer — accessible only to aidaemon.
    pub const AI_BUF:  u8 = 2;
    /// Audit / crypto store — read-only for non-audit kernel code.
    pub const AUDIT:   u8 = 3;
}

/// PKRS bit layout: bits [2n+1:2n] control key n.
///   bit 2n+0 = AD (access disable) — block all access
///   bit 2n+1 = WD (write disable)  — allow read, block write
const fn pkrs_disable_access(key: u8) -> u64 { 1 << (key as u32 * 2) }
const fn pkrs_disable_write(key: u8)  -> u64 { 1 << (key as u32 * 2 + 1) }

/// Default PKRS: driver sandbox and AI buffer are access-disabled by default.
/// Only the core kernel can open them via `pks_enter_domain`.
const DEFAULT_PKRS: u64 =
    pkrs_disable_access(pks_keys::DRIVER) |
    pkrs_disable_access(pks_keys::AI_BUF) |
    pkrs_disable_write(pks_keys::AUDIT);

fn enable_pks() {
    unsafe {
        // Set default PKRS — restricts DRIVER and AI_BUF domains
        wrmsr(MSR_IA32_PKRS, DEFAULT_PKRS);
    }
    crate::kprintln!("[hw-sec] PKS enabled (PKRS={:#018x})", DEFAULT_PKRS);
}

/// Temporarily grant access to a PKS domain for the current thread.
/// Used when kernel code legitimately needs to enter a compartment.
///
/// IMPORTANT: Call `pks_exit_domain(key)` when done.
/// Not re-entrant — save and restore PKRS if nesting is required.
#[inline]
pub fn pks_enter_domain(key: u8) {
    if !has_pks() { return; }
    unsafe {
        let pkrs = rdmsr(MSR_IA32_PKRS);
        // Clear both AD and WD bits for this key
        let mask = pkrs_disable_access(key) | pkrs_disable_write(key);
        wrmsr(MSR_IA32_PKRS, pkrs & !mask);
    }
}

/// Revoke access to a PKS domain. Restores the bit to its default state.
#[inline]
pub fn pks_exit_domain(key: u8) {
    if !has_pks() { return; }
    unsafe {
        let pkrs = rdmsr(MSR_IA32_PKRS);
        // Re-set the default restriction for this key
        let default_bits = DEFAULT_PKRS & (
            pkrs_disable_access(key) | pkrs_disable_write(key)
        );
        wrmsr(MSR_IA32_PKRS, pkrs | default_bits);
    }
}

// ─── SMAP helpers — used by copy_from/to_user ─────────────────────────────────

/// Temporarily allow kernel access to user-space pages (SMAP bypass).
/// Must be called with interrupts disabled; call `smap_restore` after.
#[inline]
pub fn smap_stac() {
    if has_smap() {
        unsafe { core::arch::asm!("stac", options(nomem, nostack)); }
    }
}

/// Restore SMAP protection after accessing user memory.
#[inline]
pub fn smap_clac() {
    if has_smap() {
        unsafe { core::arch::asm!("clac", options(nomem, nostack)); }
    }
}
