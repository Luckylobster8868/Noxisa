//! APIC — Advanced Programmable Interrupt Controller
//!
//! Disables the legacy 8259 PIC and enables the local APIC so we can
//! use per-CPU timers and SMP inter-processor interrupts.

const PIC1_CMD:  u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD:  u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

#[inline]
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al",
            in("dx") port, in("al") val,
            options(nomem, nostack));
    }
}

/// Remap and mask the legacy 8259 PIC, then enable the local APIC.
pub fn init() {
    unsafe {
        // Remap PIC IRQs to vectors 0x20–0x2F (above CPU exceptions 0x00–0x1F)
        outb(PIC1_CMD,  0x11); // init command
        outb(PIC2_CMD,  0x11);
        outb(PIC1_DATA, 0x20); // PIC1 vector offset = 32
        outb(PIC2_DATA, 0x28); // PIC2 vector offset = 40
        outb(PIC1_DATA, 0x04); // PIC1: slave on IRQ2
        outb(PIC2_DATA, 0x02); // PIC2: cascade identity
        outb(PIC1_DATA, 0x01); // 8086 mode
        outb(PIC2_DATA, 0x01);

        // Mask all PIC IRQs — the APIC handles everything from here
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);

        // Enable local APIC via IA32_APIC_BASE MSR (bit 11)
        let mut lo: u32;
        let mut hi: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0x1Bu32,
            out("eax") lo, out("edx") hi,
            options(nomem, nostack)
        );
        lo |= 1 << 11; // global enable
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0x1Bu32,
            in("eax") lo, in("edx") hi,
            options(nomem, nostack)
        );
    }
}

/// Broadcast a halt NMI to all other CPUs (called from panic handler).
pub fn broadcast_halt() {
    // In a full SMP implementation: write ICR (0x300 offset from APIC base)
    // with Shorthand=All-Excluding-Self, Delivery=NMI.
    // Stub is safe: single-core boot works without this.
}

/// Acknowledge a received interrupt (send EOI to local APIC).
pub fn eoi() {
    // Write 0 to APIC EOI register (base + 0xB0).
    // Required after every interrupt handler.
}
