//! Legacy 8259 PIC — remap + selective mask
//!
//! Deliberately separate from drivers/apic.rs. That module is an
//! unfinished stub for a future full local-APIC/IOAPIC milestone: it
//! remaps the PIC correctly but then masks BOTH PICs entirely (0xFF to
//! both data ports) on the assumption the APIC will take over IRQ
//! delivery — but nothing ever configures the IOAPIC redirection table
//! or the local APIC's LVT timer, so calling drivers::apic::init() today
//! would silently kill IRQ0/IRQ1 delivery with no replacement. This
//! module keeps the legacy 8259 path — already what idt.rs's vector
//! layout (0x20/0x21) assumes — working and minimal: remap so PIC IRQs
//! land above the CPU exception vectors, then mask every line except
//! the two we actually handle (timer, keyboard).

const PIC1_CMD:  u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD:  u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

#[inline]
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al",
            in("dx") port, in("al") val, options(nomem, nostack));
    }
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        core::arch::asm!("in al, dx",
            in("dx") port, out("al") val, options(nomem, nostack));
    }
    val
}

/// Remap the 8259 PIC pair to vectors 0x20-0x2F (above the 0x00-0x1F CPU
/// exception range idt.rs already reserves), then mask every IRQ line
/// except IRQ0 (timer, vector 0x20) and IRQ1 (keyboard, vector 0x21) —
/// the only two idt.rs currently wires a real handler for. Does NOT
/// enable interrupts (no `sti`) — that stays the caller's decision,
/// made once the scheduler is actually ready to be preempted.
pub fn init() {
    unsafe {
        // ICW1: begin init sequence, expect ICW4
        outb(PIC1_CMD, 0x11);
        outb(PIC2_CMD, 0x11);
        // ICW2: vector offsets
        outb(PIC1_DATA, 0x20); // IRQ0-7  -> vectors 0x20-0x27
        outb(PIC2_DATA, 0x28); // IRQ8-15 -> vectors 0x28-0x2F
        // ICW3: cascade wiring (PIC2 on PIC1's IRQ2 line)
        outb(PIC1_DATA, 0x04);
        outb(PIC2_DATA, 0x02);
        // ICW4: 8086 mode
        outb(PIC1_DATA, 0x01);
        outb(PIC2_DATA, 0x01);

        // Mask everything, then explicitly unmask only IRQ0 and IRQ1.
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
        let mask1 = inb(PIC1_DATA);
        outb(PIC1_DATA, mask1 & !0b0000_0011); // clear bits 0,1: IRQ0, IRQ1
    }
}
