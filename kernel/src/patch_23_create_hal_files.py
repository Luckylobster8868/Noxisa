import os
import sys

FILES = {
    "hal/mod.rs": '''pub mod console;
pub mod input;
pub mod display;
pub mod registry;

use console::SerialConsole;
use input::Ps2Keyboard;
use display::FramebufferDisplay;

static SERIAL_CONSOLE: SerialConsole = SerialConsole;
static PS2_KEYBOARD: Ps2Keyboard = Ps2Keyboard;
static FB_DISPLAY: FramebufferDisplay = FramebufferDisplay;

/// Registers the existing Noxisa drivers against the HAL registry.
/// Must run after the underlying drivers (serial, keyboard, gpu) have
/// already been initialised directly by kernel_main_native -- this does
/// not initialise hardware itself, it only wraps what's already up.
pub fn init() {
    registry::register_console(&SERIAL_CONSOLE);
    registry::register_input(&PS2_KEYBOARD);
    registry::register_display(&FB_DISPLAY);
    crate::kprintln!("[boot] HAL ready - console/input/display registered");
}
''',
    "hal/console.rs": '''/// A device that can accept formatted text output.
/// SerialConsole is a thin, zero-sized wrapper around the existing
/// drivers::serial free functions -- it owns no state of its own.
pub trait Console {
    fn write_fmt(&self, args: core::fmt::Arguments);
}

pub struct SerialConsole;

impl Console for SerialConsole {
    fn write_fmt(&self, args: core::fmt::Arguments) {
        crate::drivers::serial::write_fmt(args);
    }
}
''',
    "hal/input.rs": '''/// A device that can be polled for a single translated character.
/// Ps2Keyboard wraps drivers::keyboard's existing scancode-buffer +
/// translation functions; the IRQ1 handler in idt.rs still owns the
/// actual interrupt-driven buffering, this is a thin poll-through.
pub trait InputDevice {
    fn poll_char(&self) -> Option<u8>;
}

pub struct Ps2Keyboard;

impl InputDevice for Ps2Keyboard {
    fn poll_char(&self) -> Option<u8> {
        let sc = crate::drivers::keyboard::read_scancode()?;
        crate::drivers::keyboard::scancode_to_ascii(sc)
    }
}
''',
    "hal/display.rs": '''/// A framebuffer-style display device.
/// FramebufferDisplay wraps the existing drivers::gpu free functions.
/// Note: gpu::init() itself is NOT wrapped here -- it needs the
/// Limine-provided physical address/width/height that's only known at
/// the exact point kernel_main_native calls it, so hardware bring-up
/// stays where it is; the HAL only wraps post-init operations.
pub trait DisplayDevice {
    fn dimensions(&self) -> (u32, u32);
    fn fill_rect(&self, x: u32, y: u32, w: u32, h: u32, colour: u32);
    fn set_pixel(&self, x: u32, y: u32, colour: u32);
}

pub struct FramebufferDisplay;

impl DisplayDevice for FramebufferDisplay {
    fn dimensions(&self) -> (u32, u32) {
        crate::drivers::gpu::dimensions()
    }

    fn fill_rect(&self, x: u32, y: u32, w: u32, h: u32, colour: u32) {
        crate::drivers::gpu::fill_rect(x, y, w, h, colour);
    }

    fn set_pixel(&self, x: u32, y: u32, colour: u32) {
        crate::drivers::gpu::set_pixel(x, y, colour);
    }
}
''',
    "hal/registry.rs": '''use spin::Mutex;

use super::console::Console;
use super::input::InputDevice;
use super::display::DisplayDevice;

/// One slot per device category. Deliberately a runtime-mutable
/// registry (not compile-time-only) even though only one instance of
/// each exists today -- this is the shape that generalises correctly
/// once a genuinely different device class (NVMe/BlockDevice) needs
/// registering as the HAL's first real validation beyond input/display.
static CONSOLE: Mutex<Option<&'static dyn Console>> = Mutex::new(None);
static INPUT: Mutex<Option<&'static dyn InputDevice>> = Mutex::new(None);
static DISPLAY: Mutex<Option<&'static dyn DisplayDevice>> = Mutex::new(None);

pub fn register_console(dev: &'static dyn Console) {
    *CONSOLE.lock() = Some(dev);
}

pub fn register_input(dev: &'static dyn InputDevice) {
    *INPUT.lock() = Some(dev);
}

pub fn register_display(dev: &'static dyn DisplayDevice) {
    *DISPLAY.lock() = Some(dev);
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
''',
}

existing = [p for p in FILES if os.path.exists(p)]
if existing:
    print(f"[patch_23] ABORT: these targets already exist, nothing written: {existing}")
    sys.exit(1)

if not os.path.isdir("hal"):
    os.makedirs("hal")
    print("[patch_23] created directory: hal/")

for path, content in FILES.items():
    with open(path, "w") as f:
        f.write(content)
    print(f"[patch_23] wrote: {path}")

print("[patch_23] OK: hal/mod.rs, hal/console.rs, hal/input.rs, hal/display.rs, hal/registry.rs created")
