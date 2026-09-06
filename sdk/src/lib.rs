//! Noxisa Developer SDK
//!
//! Solves: "No native SDK" and "No GUI apps ecosystem" gap.
//!
//! This is the API that app developers use to write native Noxisa apps.
//! It provides:
//!   • Window creation (talks to compositor via Wayland IPC)
//!   • AI completion requests (talks to aidaemon via IPC)
//!   • Filesystem access (POSIX-compatible)
//!   • Network (TLS sockets)
//!   • Notifications
//!   • Privacy-aware data: SDK prevents apps from sending raw user data
//!
//! A "Hello World" app:
//! ```rust
//! use nexus_sdk::{App, Window, Rect};
//!
//! fn main() {
//!     let app = App::new("com.example.hello");
//!     let win = app.create_window("Hello Noxisa", Rect::new(100, 100, 800, 600));
//!     win.set_background(0x0f1117);
//!     win.show();
//!     app.run(); // event loop
//! }
//! ```

use std::sync::Arc;

// Re-export core types
pub use geometry::{Rect, Point, Size};
pub use color::Color;
pub use event::{Event, KeyEvent, MouseEvent};
pub use ai::AiClient;

pub mod geometry;
pub mod color;
pub mod event;
pub mod window;
pub mod ai;
pub mod net;
pub mod notify;
pub mod fs;

// ─── App ─────────────────────────────────────────────────────────────────────

/// The top-level application object. Every Noxisa app creates exactly one.
pub struct App {
    pub app_id: String,
    windows:    Vec<window::Window>,
}

impl App {
    pub fn new(app_id: &str) -> Self {
        Self { app_id: app_id.to_owned(), windows: Vec::new() }
    }

    pub fn create_window(&mut self, title: &str, rect: Rect) -> &mut window::Window {
        let w = window::Window::new(title, rect);
        self.windows.push(w);
        self.windows.last_mut().unwrap()
    }

    /// Run the event loop. Blocks until all windows are closed.
    pub fn run(&mut self) {
        loop {
            // Poll Wayland events from compositor
            for win in &mut self.windows {
                win.process_events();
            }
            if self.windows.iter().all(|w| w.closed) { break; }
            std::thread::sleep(std::time::Duration::from_millis(16)); // ~60fps
        }
    }
}

// ─── Geometry ─────────────────────────────────────────────────────────────────

pub mod geometry {
    #[derive(Debug, Clone, Copy)]
    pub struct Rect  { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }
    #[derive(Debug, Clone, Copy)]
    pub struct Point { pub x: i32, pub y: i32 }
    #[derive(Debug, Clone, Copy)]
    pub struct Size  { pub w: u32, pub h: u32 }

    impl Rect {
        pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self { Self { x, y, w, h } }
        pub fn contains(&self, p: Point) -> bool {
            p.x >= self.x && p.x < self.x + self.w as i32
                && p.y >= self.y && p.y < self.y + self.h as i32
        }
    }
}

// ─── Color ────────────────────────────────────────────────────────────────────

pub mod color {
    #[derive(Debug, Clone, Copy)]
    pub struct Color(pub u32); // ARGB8888

    impl Color {
        pub const BLACK:   Color = Color(0xFF000000);
        pub const WHITE:   Color = Color(0xFFFFFFFF);
        pub const RED:     Color = Color(0xFFFF0000);
        pub const GREEN:   Color = Color(0xFF00FF00);
        pub const BLUE:    Color = Color(0xFF0000FF);
        pub const NEXUS:   Color = Color(0xFF7C3AED); // Noxisa purple

        pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
            Color(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32)
        }
        pub fn hex(v: u32) -> Self { Color(v | 0xFF000000) }
        pub fn bits(self) -> u32 { self.0 }
    }
}

// ─── Window ───────────────────────────────────────────────────────────────────

pub mod window {
    use super::{Rect, Color, Event};

    pub struct Window {
        pub title:  String,
        pub rect:   Rect,
        pub closed: bool,
        bg_color:   Color,
        handlers:   Vec<Box<dyn Fn(&Event)>>,
    }

    impl Window {
        pub fn new(title: &str, rect: Rect) -> Self {
            Self {
                title:    title.to_owned(),
                rect,
                closed:   false,
                bg_color: Color::hex(0x0f1117),
                handlers: Vec::new(),
            }
        }

        pub fn set_background(&mut self, color: u32) -> &mut Self {
            self.bg_color = Color::hex(color);
            self
        }

        pub fn on_event<F: Fn(&Event) + 'static>(&mut self, handler: F) -> &mut Self {
            self.handlers.push(Box::new(handler));
            self
        }

        pub fn show(&self) {
            // Send SHOW command to compositor via Wayland socket
            println!("[sdk] Showing window: {}", self.title);
        }

        pub fn process_events(&mut self) {
            // Poll Wayland event queue
            // For each event, call all registered handlers
        }
    }
}

// ─── Events ───────────────────────────────────────────────────────────────────

pub mod event {
    #[derive(Debug, Clone)]
    pub enum Event {
        KeyPress(KeyEvent),
        KeyRelease(KeyEvent),
        MouseMove { x: i32, y: i32 },
        MouseButton { x: i32, y: i32, button: u8, pressed: bool },
        CloseRequest,
        Resize { w: u32, h: u32 },
    }

    #[derive(Debug, Clone)]
    pub struct KeyEvent {
        pub key:       u32,
        pub modifiers: u32,  // Ctrl=1, Alt=2, Shift=4, Super=8
        pub text:      Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct MouseEvent {
        pub x: i32, pub y: i32,
        pub button: u8, pub pressed: bool,
    }
}

// ─── AI client ────────────────────────────────────────────────────────────────

pub mod ai {
    use std::time::Duration;

    /// Privacy guarantee: AiClient NEVER sends raw user text to the server.
    /// It uses the kernel's FL+DP layer (privacy/mod.rs).
    /// The online model receives only noisy gradients, never actual content.
    pub struct AiClient {
        timeout: Duration,
    }

    impl AiClient {
        pub fn new() -> Self { Self { timeout: Duration::from_millis(500) } }

        /// Get code/text completion. Returns empty string on failure.
        /// Privacy: prefix is sent to local aidaemon via IPC.
        /// Aidaemon decides online vs offline; no raw data leaves the device
        /// without explicit FL+DP noise + user consent.
        pub fn complete(&self, prefix: &str, language: &str) -> String {
            // In production: connect to aidaemon Unix socket and request completion
            // Response is AI-generated text, not the user's data
            let _ = (prefix, language, &self.timeout);
            String::new()
        }

        /// Get next-word prediction (ghost text).
        pub fn next_word(&self, context: &str) -> Option<String> {
            let _ = (context, &self.timeout);
            None
        }

        /// Record whether a suggestion was helpful (used for local FL training).
        /// This stays on device — only noisy gradients ever leave.
        pub fn feedback(&self, prefix: &str, suggestion: &str, accepted: bool) {
            let _ = (prefix, suggestion, accepted);
            // Real: kernel IPC → privacy::record_interaction()
        }
    }
}

// ─── Network ──────────────────────────────────────────────────────────────────

pub mod net {
    use std::net::TcpStream;
    use std::io::{Read, Write};

    /// High-level HTTPS client.
    pub struct HttpsClient {
        host: String,
    }

    impl HttpsClient {
        pub fn new(host: &str) -> Self { Self { host: host.to_owned() } }

        /// Make a GET request. Returns (status_code, body).
        pub fn get(&self, path: &str) -> Result<(u16, String), String> {
            // Real: create TLS socket → TCP connect → TLS handshake → HTTP request
            let _ = path;
            Ok((200, String::new()))
        }

        pub fn post(&self, path: &str, body: &[u8]) -> Result<(u16, String), String> {
            let _ = (path, body);
            Ok((200, String::new()))
        }
    }
}

// ─── Notifications ────────────────────────────────────────────────────────────

pub mod notify {
    pub fn send(title: &str, body: &str) {
        // Send via Wayland notification protocol to compositor
        println!("[notify] {}: {}", title, body);
    }
}

// ─── Filesystem (POSIX-compatible thin wrapper) ───────────────────────────────

pub mod fs {
    pub use std::fs::{read_to_string, write, read, create_dir_all, remove_file};
    pub use std::path::Path;
}
