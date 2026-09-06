//! Noxisa Compositor — Wayland window manager
//!
//! Best-of-all-worlds features:
//!   macOS:   smooth animations, Spaces (virtual desktops), blur, transparency
//!   Windows: 12-zone snapping (Windows 11-style + thirds), taskbar
//!   i3/Sway: BSP tiling mode, keyboard-driven, no gaps waste
//!   KDE:     window rules, effects
//!   GNOME:   activities overview, hot corners

use std::collections::VecDeque;
use std::time::Instant;

// ─── Geometry ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self { Self { x, y, w, h } }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w as i32
            && py >= self.y && py < self.y + self.h as i32
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w as i32 / 2, self.y + self.h as i32 / 2)
    }
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::new(
        lerp(a.x as f32, b.x as f32, t) as i32,
        lerp(a.y as f32, b.y as f32, t) as i32,
        lerp(a.w as f32, b.w as f32, t) as u32,
        lerp(a.h as f32, b.h as f32, t) as u32,
    )
}
fn lerp(a: f32, b: f32, t: f32) -> f32 { a + (b - a) * t }
fn ease_out_cubic(t: f32) -> f32 { 1.0 - (1.0 - t).powi(3) }

// ─── Snap zones (12 positions) ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapZone {
    Left, Right, Top, Bottom,
    TopLeft, TopRight, BottomLeft, BottomRight,
    Center,
    ThirdLeft, ThirdMid, ThirdRight,
}

fn snap_rect(zone: SnapZone, screen: Rect) -> Rect {
    let (sx, sy, sw, sh) = (screen.x, screen.y, screen.w, screen.h);
    let hw = sw / 2;
    let hh = sh / 2;
    let tw = sw / 3;
    match zone {
        SnapZone::Left        => Rect::new(sx,                sy,      hw,   sh),
        SnapZone::Right       => Rect::new(sx + hw as i32,    sy,      hw,   sh),
        SnapZone::Top         => Rect::new(sx,                sy,      sw,   hh),
        SnapZone::Bottom      => Rect::new(sx,   sy + hh as i32,       sw,   hh),
        SnapZone::TopLeft     => Rect::new(sx,                sy,      hw,   hh),
        SnapZone::TopRight    => Rect::new(sx + hw as i32,    sy,      hw,   hh),
        SnapZone::BottomLeft  => Rect::new(sx,   sy + hh as i32,       hw,   hh),
        SnapZone::BottomRight => Rect::new(sx + hw as i32, sy + hh as i32, hw, hh),
        SnapZone::Center      => Rect::new(sx + (sw / 8) as i32, sy + (sh / 8) as i32,
                                           sw * 3 / 4, sh * 3 / 4),
        SnapZone::ThirdLeft   => Rect::new(sx,                sy,      tw,   sh),
        SnapZone::ThirdMid    => Rect::new(sx + tw as i32,    sy,      tw,   sh),
        SnapZone::ThirdRight  => Rect::new(sx + 2*tw as i32,  sy,      tw,   sh),
    }
}

/// Detect snap zone from pointer position near screen edges.
fn detect_snap(px: i32, py: i32, screen: Rect) -> Option<SnapZone> {
    let t     = 24i32; // threshold in pixels
    let right = screen.x + screen.w as i32;
    let bot   = screen.y + screen.h as i32;
    let near_l = px < screen.x + t;
    let near_r = px > right - t;
    let near_t = py < screen.y + t;
    let near_b = py > bot   - t;
    match (near_l, near_r, near_t, near_b) {
        (true,  false, true,  false) => Some(SnapZone::TopLeft),
        (false, true,  true,  false) => Some(SnapZone::TopRight),
        (true,  false, false, true)  => Some(SnapZone::BottomLeft),
        (false, true,  false, true)  => Some(SnapZone::BottomRight),
        (true,  false, false, false) => Some(SnapZone::Left),
        (false, true,  false, false) => Some(SnapZone::Right),
        (false, false, true,  false) => Some(SnapZone::Top),
        (false, false, false, true)  => Some(SnapZone::Bottom),
        _ => None,
    }
}

// ─── Window ───────────────────────────────────────────────────────────────────

pub type Wid = u32;

#[derive(Debug, Clone)]
pub struct Window {
    pub id:       Wid,
    pub title:    String,
    pub app_id:   String,
    pub rect:     Rect,
    pub visible:  bool,
    pub focused:  bool,
    pub floating: bool,
    pub opacity:  f32,
    pub snap:     Option<SnapZone>,
    pub desktop:  u32,

    // Animation state
    anim_from:    Option<Rect>,
    anim_to:      Option<Rect>,
    anim_t:       f32,   // 0.0 → 1.0
    anim_speed:   f32,   // multiplier
}

impl Window {
    pub fn new(id: Wid, title: &str, app_id: &str, rect: Rect, desktop: u32) -> Self {
        Self {
            id, title: title.to_owned(), app_id: app_id.to_owned(),
            rect, visible: true, focused: false, floating: true,
            opacity: 1.0, snap: None, desktop,
            anim_from: Some(Rect::new(rect.center().0, rect.center().1, 0, 0)),
            anim_to:   Some(rect),
            anim_t:    0.0, anim_speed: 4.0,
        }
    }

    /// Advance animation by `dt` seconds. Returns true while animating.
    pub fn tick(&mut self, dt: f32) -> bool {
        if self.anim_from.is_none() { return false; }
        self.anim_t += dt * self.anim_speed;
        let t = ease_out_cubic(self.anim_t.min(1.0));
        if let (Some(from), Some(to)) = (self.anim_from, self.anim_to) {
            self.rect = lerp_rect(from, to, t);
        }
        if self.anim_t >= 1.0 {
            self.anim_from = None;
            self.anim_to   = None;
            false
        } else {
            true
        }
    }

    /// Start an animated move to `target`.
    pub fn animate_to(&mut self, target: Rect) {
        self.anim_from = Some(self.rect);
        self.anim_to   = Some(target);
        self.anim_t    = 0.0;
    }
}

// ─── BSP tiling (i3-style) ────────────────────────────────────────────────────

/// Recursively partition `area` into `n` equal slices, alternating H/V split.
/// Time: O(n),  Space: O(n)
fn bsp_rects(area: Rect, n: usize, gap: i32) -> Vec<Rect> {
    if n == 0 { return vec![]; }
    if n == 1 { return vec![area]; }

    let (a, b) = if area.w >= area.h {
        let mid = area.w / 2;
        (
            Rect::new(area.x, area.y, mid.saturating_sub(gap as u32 / 2), area.h),
            Rect::new(area.x + mid as i32, area.y, area.w - mid, area.h),
        )
    } else {
        let mid = area.h / 2;
        (
            Rect::new(area.x, area.y, area.w, mid.saturating_sub(gap as u32 / 2)),
            Rect::new(area.x, area.y + mid as i32, area.w, area.h - mid),
        )
    };
    let left  = n / 2;
    let right = n - left;
    let mut r = bsp_rects(a, left, gap);
    r.extend(bsp_rects(b, right, gap));
    r
}

// ─── Virtual desktops ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Desktop {
    pub id:   u32,
    pub name: String,
}

// ─── Compositor ───────────────────────────────────────────────────────────────

pub struct Compositor {
    windows:     Vec<Window>,
    next_wid:    Wid,
    desktops:    Vec<Desktop>,
    active_desk: u32,
    focused:     Option<Wid>,
    screen:      Rect,
    cursor:      (i32, i32),
    gap:         i32,
    tiling:      bool,
    last_tick:   Instant,
}

impl Compositor {
    pub fn new(screen: Rect) -> Self {
        let desktops = (0..4).map(|i| Desktop {
            id:   i,
            name: format!("Desktop {}", i + 1),
        }).collect();
        Self {
            windows: Vec::new(), next_wid: 1,
            desktops, active_desk: 0,
            focused: None, screen, cursor: (0, 0),
            gap: 4, tiling: false, last_tick: Instant::now(),
        }
    }

    pub fn create_window(&mut self, title: &str, app_id: &str, rect: Rect) -> Wid {
        let id = self.next_wid;
        self.next_wid += 1;
        self.windows.push(Window::new(id, title, app_id, rect, self.active_desk));
        self.focus(id);
        if self.tiling { self.retile(); }
        id
    }

    pub fn close_window(&mut self, id: Wid) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.visible = false;
        }
        self.windows.retain(|w| w.visible);
        if self.tiling { self.retile(); }
    }

    pub fn focus(&mut self, id: Wid) {
        for w in &mut self.windows { w.focused = false; }
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.focused = true;
        }
        self.focused = Some(id);
    }

    pub fn move_pointer(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
    }

    pub fn move_window(&mut self, id: Wid, dx: i32, dy: i32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.snap = None;
            w.rect.x += dx;
            w.rect.y += dy;
            // Show snap ghost
            let (px, py) = (w.rect.x, w.rect.y);
            w.snap = detect_snap(px, py, self.screen);
        }
    }

    pub fn release_window(&mut self, id: Wid) {
        if let Some(snap) = self.windows.iter().find(|w| w.id == id).and_then(|w| w.snap) {
            let target = snap_rect(snap, self.screen);
            if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
                w.animate_to(target);
            }
        }
    }

    pub fn snap(&mut self, id: Wid, zone: SnapZone) {
        let target = snap_rect(zone, self.screen);
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.animate_to(target);
            w.snap = Some(zone);
        }
    }

    pub fn toggle_tiling(&mut self) {
        self.tiling = !self.tiling;
        if self.tiling { self.retile(); }
    }

    /// Re-tile all floating windows on the active desktop using BSP.
    pub fn retile(&mut self) {
        let screen  = self.screen;
        let gap     = self.gap;
        let desk    = self.active_desk;
        let indices: Vec<usize> = self.windows.iter().enumerate()
            .filter(|(_, w)| w.visible && w.desktop == desk)
            .map(|(i, _)| i)
            .collect();
        let n = indices.len();
        if n == 0 { return; }
        let rects = bsp_rects(screen, n, gap);
        for (&idx, rect) in indices.iter().zip(rects.iter()) {
            self.windows[idx].animate_to(*rect);
            self.windows[idx].floating = false;
        }
    }

    pub fn switch_desktop(&mut self, n: u32) {
        if n < self.desktops.len() as u32 {
            self.active_desk = n;
        }
    }

    /// Advance all animations. Call at display refresh rate (60 Hz → dt = 1/60).
    pub fn tick(&mut self, dt: f32) {
        for w in &mut self.windows {
            w.tick(dt);
        }
    }

    pub fn windows_on_active_desktop(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(move |w| w.visible && w.desktop == self.active_desk)
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let screen  = Rect::new(0, 0, 1920, 1080);
    let mut comp = Compositor::new(screen);

    log::info!("[compositor] Starting Noxisa compositor on {}×{}", screen.w, screen.h);

    // Load Lua config
    let cfg_engine = config_engine::ConfigEngine::new()?;
    cfg_engine.load_user_config()?;

    log::info!("[compositor] Config loaded");

    // Create test windows for demonstration
    let id1 = comp.create_window("Terminal", "nexus.terminal", Rect::new(100, 100, 800, 600));
    let id2 = comp.create_window("Firefox", "org.mozilla.firefox", Rect::new(200, 150, 1200, 800));
    comp.snap(id2, SnapZone::Right);

    log::info!("[compositor] Windows created. id1={id1} id2={id2}");

    // Main loop (60 Hz)
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(16));
    loop {
        interval.tick().await;
        comp.tick(1.0 / 60.0);
    }
}
