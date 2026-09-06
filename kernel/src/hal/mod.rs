pub mod console;
pub mod input;
pub mod display;
pub mod block;
pub mod registry;

use console::SerialConsole;
use input::Ps2Keyboard;
use display::FramebufferDisplay;
use block::NvmeBlockDevice;

static SERIAL_CONSOLE: SerialConsole = SerialConsole;
static PS2_KEYBOARD: Ps2Keyboard = Ps2Keyboard;
static FB_DISPLAY: FramebufferDisplay = FramebufferDisplay;
static NVME_BLOCK: NvmeBlockDevice = NvmeBlockDevice;

/// Registers the existing Noxisa drivers against the HAL registry.
/// Must run after the underlying drivers (serial, keyboard, gpu) have
/// already been initialised directly by kernel_main_native -- this does
/// not initialise hardware itself, it only wraps what's already up.
///
/// NvmeBlockDevice is registered unconditionally, even if no NVMe
/// controller is actually present - it does no PCI probing itself.
/// Absence is handled lazily: the first real read_block()/write_block()
/// call will surface NvmeInitError::NoController, mapped to
/// BlockError::NotReady, exactly like every other lazy-init path in
/// this codebase (ensure_admin_queue() etc.). This keeps hal::init()
/// itself simple and matches the registry's Option-based design,
/// which is meant to degrade gracefully when a device class is absent.
pub fn init() {
    registry::register_console(&SERIAL_CONSOLE);
    registry::register_input(&PS2_KEYBOARD);
    registry::register_display(&FB_DISPLAY);
    registry::register_block(&NVME_BLOCK);
    crate::kprintln!("[boot] HAL ready - console/input/display/block registered");
}
