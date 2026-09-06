use spin::Mutex;

use super::console::Console;
use super::input::InputDevice;
use super::display::DisplayDevice;
use super::block::{BlockDevice, BlockError};

/// One slot per device category. Deliberately a runtime-mutable
/// registry (not compile-time-only) even though only one instance of
/// each exists today -- this is the shape that generalises correctly
/// once a genuinely different device class (NVMe/BlockDevice) needs
/// registering as the HAL's first real validation beyond input/display.
static CONSOLE: Mutex<Option<&'static dyn Console>> = Mutex::new(None);
static INPUT: Mutex<Option<&'static dyn InputDevice>> = Mutex::new(None);
static DISPLAY: Mutex<Option<&'static dyn DisplayDevice>> = Mutex::new(None);
static BLOCK: Mutex<Option<&'static dyn BlockDevice>> = Mutex::new(None);

pub fn register_console(dev: &'static dyn Console) {
    *CONSOLE.lock() = Some(dev);
}

pub fn register_input(dev: &'static dyn InputDevice) {
    *INPUT.lock() = Some(dev);
}

pub fn register_display(dev: &'static dyn DisplayDevice) {
    *DISPLAY.lock() = Some(dev);
}

pub fn register_block(dev: &'static dyn BlockDevice) {
    *BLOCK.lock() = Some(dev);
}

pub fn console_write_fmt(args: core::fmt::Arguments) {
    if let Some(c) = *CONSOLE.lock() {
        c.write_fmt(args);
    }
}

pub fn input_poll_char() -> Option<u8> {
    INPUT.lock().and_then(|d| d.poll_char())
}

pub fn display_dimensions() -> Option<(u32, u32)> {
    DISPLAY.lock().map(|d| d.dimensions())
}

pub fn display_fill_rect(x: u32, y: u32, w: u32, h: u32, colour: u32) -> bool {
    if let Some(d) = *DISPLAY.lock() {
        d.fill_rect(x, y, w, h, colour);
        true
    } else {
        false
    }
}

/// Returns 0 if no block device is registered - matches how
/// display_dimensions()/input_poll_char() degrade gracefully via
/// Option rather than panicking when a device class is absent.
pub fn block_size() -> u32 {
    BLOCK.lock().map(|d| d.block_size()).unwrap_or(0)
}

pub fn block_count() -> u64 {
    BLOCK.lock().map(|d| d.block_count()).unwrap_or(0)
}

pub fn block_read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    match *BLOCK.lock() {
        Some(d) => d.read_block(lba, buf),
        None => Err(BlockError::NotReady),
    }
}

pub fn block_write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    match *BLOCK.lock() {
        Some(d) => d.write_block(lba, buf),
        None => Err(BlockError::NotReady),
    }
}
