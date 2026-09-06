/// A device that can be polled for a single translated character.
/// Ps2Keyboard wraps drivers::keyboard's existing scancode-buffer +
/// translation functions; the IRQ1 handler in idt.rs still owns the
/// actual interrupt-driven buffering, this is a thin poll-through.
pub trait InputDevice: Sync {
    fn poll_char(&self) -> Option<u8>;
}

pub struct Ps2Keyboard;

impl InputDevice for Ps2Keyboard {
    fn poll_char(&self) -> Option<u8> {
        let sc = crate::drivers::keyboard::read_scancode()?;
        crate::drivers::keyboard::scancode_to_ascii(sc)
    }
}
