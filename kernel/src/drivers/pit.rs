//! PIT — 8253 Programmable Interval Timer
//! Sets up IRQ0 at ~100 Hz (10ms tick) for preemptive scheduling.

use core::sync::atomic::{AtomicU64, Ordering};

const PIT_CH0:  u16 = 0x40;
const PIT_CMD:  u16 = 0x43;
const PIT_HZ:   u32 = 100; // 100 Hz = 10ms tick
const PIT_BASE: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al",
        in("dx") port, in("al") val, options(nomem, nostack));
}

/// Initialise PIT channel 0 at 100 Hz.
pub fn init() {
    let divisor = (PIT_BASE / PIT_HZ) as u16;
    unsafe {
        outb(PIT_CMD, 0x36); // channel 0, lobyte/hibyte, square wave
        outb(PIT_CH0, (divisor & 0xFF) as u8);
        outb(PIT_CH0, (divisor >> 8) as u8);
    }

}

/// Called from IRQ0 handler — increment tick counter.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 { TICKS.load(Ordering::Relaxed) }
pub fn ms() -> u64 { TICKS.load(Ordering::Relaxed) * 10 }
