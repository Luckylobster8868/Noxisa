/// A framebuffer-style display device.
/// FramebufferDisplay wraps the existing drivers::gpu free functions.
/// Note: gpu::init() itself is NOT wrapped here -- it needs the
/// Limine-provided physical address/width/height that's only known at
/// the exact point kernel_main_native calls it, so hardware bring-up
/// stays where it is; the HAL only wraps post-init operations.
pub trait DisplayDevice: Sync {
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
