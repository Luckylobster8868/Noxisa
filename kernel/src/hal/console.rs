/// A device that can accept formatted text output.
/// SerialConsole is a thin, zero-sized wrapper around the existing
/// drivers::serial free functions -- it owns no state of its own.
pub trait Console: Sync {
    fn write_fmt(&self, args: core::fmt::Arguments);
}

pub struct SerialConsole;

impl Console for SerialConsole {
    fn write_fmt(&self, args: core::fmt::Arguments) {
        crate::drivers::serial::write_fmt(args);
    }
}
