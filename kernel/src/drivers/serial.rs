//! UART 16550 — COM1 (I/O base 0x3F8)
//!
//! Available before the heap is set up.  Used for all early kernel logging
//! and is the backing implementation of `kprint!` / `kprintln!`.

use spin::Mutex;
use core::fmt;

const BASE: u16 = 0x3F8;

/// Read a byte from an x86 I/O port.
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe {
        core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack));
    }
    v
}

/// Write a byte to an x86 I/O port.
#[inline]
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
    }
}

struct Uart;

static UART: Mutex<Uart> = Mutex::new(Uart);

impl Uart {
    fn init(&self) {
        unsafe {
            outb(BASE + 1, 0x00); // disable interrupts
            outb(BASE + 3, 0x80); // DLAB on
            outb(BASE + 0, 0x01); // divisor = 1 → 115200 baud (low byte)
            outb(BASE + 1, 0x00); // divisor high byte
            outb(BASE + 3, 0x03); // 8N1, DLAB off
            outb(BASE + 2, 0xC7); // FIFO on, clear, 14-byte threshold
            outb(BASE + 4, 0x0B); // RTS + DSR on
        }
    }

    fn send(&self, byte: u8) {
        // Busy-wait for transmit holding register to be empty (bit 5 of LSR)
        unsafe {
            while inb(BASE + 5) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            outb(BASE + 0, byte);
        }
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            // Convert LF → CRLF for serial terminals
            if b == b'\n' { self.send(b'\r'); }
            self.send(b);
        }
        Ok(())
    }
}

/// Read a byte from serial — returns None if no data available (non-blocking).
pub fn try_read() -> Option<u8> {
    unsafe {
        if inb(BASE + 5) & 0x01 != 0 {
            Some(inb(BASE + 0))
        } else {
            None
        }
    }
}

/// Read a byte — blocking, spins until data arrives.
pub fn read_byte() -> u8 {
    loop {
        if let Some(b) = try_read() { return b; }
        core::hint::spin_loop();
    }
}

/// Initialise COM1 at 115200 baud, 8N1.  Call once before logging.
pub fn init() {
    UART.lock().init();
}

/// Write a formatted string to serial.  Called by `kprint!`.
pub fn write_fmt(args: fmt::Arguments) {
    use fmt::Write;
    // Ignore errors — serial is best-effort in kernel context.
    UART.lock().write_fmt(args).ok();
}

/// Lock-free emergency write path — used ONLY by the panic handler.
///
/// write_fmt() above holds UART's Mutex for the entire duration of
/// formatting every argument (the `.lock()` temporary lives for the whole
/// statement). If anything panics mid-format while that lock is held, the
/// guard's Drop never runs (this kernel has no unwinding - `panic = abort` -
/// so there is no unwind pass to run destructors during), permanently
/// orphaning the lock. If the panic handler then tries to report itself via
/// the normal kprintln!()/write_fmt() path, it deadlocks trying to
/// re-acquire a lock that will never be released - turning a real bug into
/// a silent hang with no error message at all (confirmed live via gdb: a
/// panic's own attempt to print was found spinning forever on UART's
/// `lock cmpxchg`). A panic handler must never be able to deadlock trying
/// to report a panic, so this bypasses the Mutex entirely and writes
/// straight to the UART port - safe here specifically because the panic
/// handler already did `cli` and never returns, so there is no possibility
/// of this racing the normal locked path from another context.
struct RawRacyUart;
impl fmt::Write for RawRacyUart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                unsafe {
                    while inb(BASE + 5) & 0x20 == 0 { core::hint::spin_loop(); }
                    outb(BASE + 0, b'\r');
                }
            }
            unsafe {
                while inb(BASE + 5) & 0x20 == 0 { core::hint::spin_loop(); }
                outb(BASE + 0, b);
            }
        }
        Ok(())
    }
}

pub fn write_fmt_panic(args: fmt::Arguments) {
    use fmt::Write;
    RawRacyUart.write_fmt(args).ok();
}
