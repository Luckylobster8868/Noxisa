//! PS/2 Keyboard driver — IRQ1, scancode set 1

use core::sync::atomic::{AtomicBool, Ordering};

const KB_DATA: u16 = 0x60;
const KB_STATUS: u16 = 0x64;

/// Ring buffer for scancodes
static mut KEYBUF: [u8; 256] = [0u8; 256];
static mut HEAD: u8 = 0;
static mut TAIL: u8 = 0;

unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack));
    v
}

/// Called from IRQ1 handler — read scancode and buffer it.
pub fn handle_irq() {
    unsafe {
        let sc = inb(KB_DATA);
        let next = HEAD.wrapping_add(1);
        if next != TAIL {
            KEYBUF[HEAD as usize] = sc;
            HEAD = next;
        }
    }
}

/// Read next scancode from buffer (non-blocking). Returns None if empty.
pub fn read_scancode() -> Option<u8> {
    unsafe {
        if HEAD == TAIL { return None; }
        let sc = KEYBUF[TAIL as usize];
        TAIL = TAIL.wrapping_add(1);
        Some(sc)
    }
}

/// Translate scancode set 1 to ASCII (key-down events only).
pub fn scancode_to_ascii(sc: u8) -> Option<u8> {
    // Only handle key-down (bit 7 = 0)
    if sc & 0x80 != 0 { return None; }
    let ascii: &[u8] = b"\x00\x1B1234567890-=\x08\tqwertyuiop[]\n\x00asdfghjkl;'`\x00\\zxcvbnm,./\x00*\x00 ";
    if (sc as usize) < ascii.len() {
        let c = ascii[sc as usize];
        if c != 0 { Some(c) } else { None }
    } else {
        None
    }
}
