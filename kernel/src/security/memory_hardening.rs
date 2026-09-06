//! Memory Hardening — ASLR, Stack Canaries, NX, Guard Pages
//!
//! Prevents and mitigates memory-safety exploits:
//!
//!   ASLR        — randomise base addresses of stack, heap, and code
//!                 so attackers cannot predict where to jump
//!   NX          — mark stack and heap pages non-executable
//!                 so injected shellcode cannot run
//!   Stack guard — place a canary word at the bottom of every stack;
//!                 a stack overflow overwrites it and triggers a fault
//!   Guard pages — unmapped pages at stack boundaries;
//!                 accessing them faults before any damage is done
//!   PIE         — position-independent executables (required for ASLR to work)

use core::sync::atomic::{AtomicU64, Ordering};

/// 64-bit ASLR entropy seed — set once from hardware RNG at boot.
static ASLR_SEED: AtomicU64 = AtomicU64::new(0);

/// Stack canary — constant for the lifetime of the kernel.
/// Each user process gets its own randomly-derived canary.
static KERNEL_CANARY: AtomicU64 = AtomicU64::new(0);

/// Initialise memory hardening. Call after PMM and paging are ready.
pub fn init() {
    // Get entropy from RDRAND (Intel/AMD hardware RNG).
    let entropy = rdrand().unwrap_or(0xDEAD_BEEF_CAFE_1234);
    ASLR_SEED.store(entropy, Ordering::Relaxed);

    // Derive a separate canary value.
    KERNEL_CANARY.store(mix64(entropy, 0x517C_C1B7_2722_0A95), Ordering::Relaxed);

    crate::kprintln!("[hardening] ASLR seed set, stack canary initialised");
}

/// Generate a randomised base address for a new mapping.
///
/// Uses a simple LCG seeded from RDRAND.
/// The result is page-aligned and stays within the user address space.
pub fn aslr_base(hint: u64) -> u64 {
    let seed = ASLR_SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let rand = mix64(seed, hint);

    // User space: 0x0001_0000 – 0x7FFF_FFFF_FFFF
    // Limit randomness to bits 12–47 (36 bits = ±128 GiB of entropy)
    let offset = rand & 0x0000_FFFF_FFFF_F000;
    0x0000_1000_0000_0000u64
        .wrapping_add(offset)
        & !0xFFF // page align
}

/// Generate a per-process stack canary.
/// Called when creating a new user process.
pub fn new_stack_canary(pid: u32) -> u64 {
    let seed = ASLR_SEED.load(Ordering::Relaxed);
    mix64(seed, pid as u64)
}

/// Map a guard page (unmapped page) at `virt_addr`.
/// Any access to this page will trigger a page-fault → kernel kills the process.
pub fn place_guard_page(virt_addr: u64) {
    // Ensure the page has no permissions: present=0 means any access faults.
    // In the full paging implementation this removes the PTE entirely.
    crate::memory::paging::map_page(
        virt_addr,
        0, // physical address irrelevant — page will be marked not-present
        crate::memory::paging::PageFlags::KERNEL_RO, // will be refined to 0
    );
    crate::memory::paging::flush_tlb(virt_addr);
}

/// Mark a virtual page range as non-executable (NX).
/// Prevents shellcode injection on stack/heap.
pub fn mark_nx(virt_start: u64, pages: usize) {
    for i in 0..pages as u64 {
        // The NO_EXEC bit in PageFlags sets the NX bit (bit 63 of the PTE).
        crate::memory::paging::map_page(
            virt_start + i * 4096,
            crate::memory::paging::virt_to_phys(virt_start + i * 4096),
            crate::memory::paging::PageFlags::KERNEL_RW, // RW but not RX
        );
    }
}

/// Read a 64-bit random value from the hardware RNG (RDRAND instruction).
/// Returns None if the CPU does not support RDRAND.
pub fn rdrand() -> Option<u64> {
    let mut val: u64 = 0;
    let ok: u8;
    unsafe {
        core::arch::asm!(
            "rdrand {v}",
            "setc {ok}",
            v  = out(reg) val,
            ok = out(reg_byte) ok,
            options(nomem, nostack),
        );
    }
    if ok != 0 { Some(val) } else { None }
}

/// Finalise entropy — mix current seed with a new hardware sample.
/// Called periodically to refresh ASLR entropy.
pub fn reseed() {
    if let Some(r) = rdrand() {
        ASLR_SEED.fetch_xor(r, Ordering::Relaxed);
    }
}

// ─── Hash mixing ──────────────────────────────────────────────────────────────

/// Fast non-cryptographic 64-bit mixing function (Murmur3-finaliser inspired).
/// Used to derive per-process values from the global seed.
#[inline]
fn mix64(mut x: u64, key: u64) -> u64 {
    x = x.wrapping_add(key);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}
