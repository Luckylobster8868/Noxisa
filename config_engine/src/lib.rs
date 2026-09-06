//! Config Engine — Embeds Lua 5.4 via mlua.
//!
//! Loads ~/.config/nexus/init.lua (or the default config) and
//! provides the compositor with keybindings, themes, window rules, etc.

use anyhow::{Context, Result};
use mlua::{Lua, Value, Table};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

// ─── Config output types ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg:      String,
    pub fg:      String,
    pub accent:  String,
    pub border:  String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg:     "#0f1117".into(),
            fg:     "#e2e8f0".into(),
            accent: "#7c3aed".into(),
            border: "#2d3748".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub keys:   String,
    pub action: String,
}

#[derive(Debug, Clone, Default)]
pub struct CompositorConfig {
    pub theme:        Theme,
    pub gap:          u32,
    pub border_width: u32,
    pub border_radius:u32,
    pub animations:   bool,
    pub anim_speed:   f32,
    pub blur:         bool,
    pub blur_radius:  u32,
    pub layout:       String,
    pub desktop_count:u32,
    pub keybindings:  Vec<KeyBinding>,
}

// ─── Config engine ────────────────────────────────────────────────────────────

pub struct ConfigEngine {
    lua:         Lua,
    config:      Arc<RwLock<CompositorConfig>>,
    config_path: PathBuf,
}

impl ConfigEngine {
    /// Create and initialise. Loads the stdlib but NOT the user config yet.
    pub fn new() -> Result<Self> {
        let lua = Lua::new();

        // Default config
        let config = Arc::new(RwLock::new(CompositorConfig {
            gap:           4,
            border_width:  1,
            border_radius: 8,
            animations:    true,
            anim_speed:    1.0,
            blur:          true,
            blur_radius:   10,
            layout:        "float".into(),
            desktop_count: 4,
            theme:         Theme::default(),
            keybindings:   Vec::new(),
        }));

        let config_path = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/etc"))
            .join("nexus/init.lua");

        let engine = Self { lua, config, config_path };
        engine.register_api()?;
        engine.load_stdlib()?;
        Ok(engine)
    }

    /// Register nexus.* API functions into the Lua VM.
    fn register_api(&self) -> Result<()> {
        let cfg = self.config.clone();
        let nexus = self.lua.create_table()?;

        // nexus.theme(name) or nexus.theme(name, overrides)
        {
            let cfg2 = cfg.clone();
            nexus.set("_apply_theme", self.lua.create_function(move |_, t: Table| {
                let mut c = cfg2.write();
                if let Ok(bg)     = t.get::<Value>("bg")     { if let Value::String(s) = bg     { c.theme.bg      = s.to_str().unwrap_or("").to_owned(); } }
                if let Ok(fg)     = t.get::<Value>("fg")     { if let Value::String(s) = fg     { c.theme.fg      = s.to_str().unwrap_or("").to_owned(); } }
                if let Ok(accent) = t.get::<Value>("accent") { if let Value::String(s) = accent { c.theme.accent  = s.to_str().unwrap_or("").to_owned(); } }
                if let Ok(border) = t.get::<Value>("border") { if let Value::String(s) = border { c.theme.border  = s.to_str().unwrap_or("").to_owned(); } }
                Ok(())
            })?)?;
        }

        // nexus._set(key, value)
        {
            let cfg2 = cfg.clone();
            nexus.set("_set", self.lua.create_function(move |_, (k, v): (String, Value)| {
                let mut c = cfg2.write();
                match k.as_str() {
                    "gap"            => if let Value::Integer(n) = v { c.gap = n as u32; }
                    "border_width"   => if let Value::Integer(n) = v { c.border_width = n as u32; }
                    "border_radius"  => if let Value::Integer(n) = v { c.border_radius = n as u32; }
                    "animations"     => if let Value::Boolean(b) = v { c.animations = b; }
                    "anim_speed"     => if let Value::Number(f) = v  { c.anim_speed = f as f32; }
                    "blur"           => if let Value::Boolean(b) = v { c.blur = b; }
                    "blur_radius"    => if let Value::Integer(n) = v { c.blur_radius = n as u32; }
                    "layout"         => if let Value::String(s) = v  { c.layout = s.to_str().unwrap_or("float").to_owned(); }
                    "desktop_count"  => if let Value::Integer(n) = v { c.desktop_count = n as u32; }
                    _ => {}
                }
                Ok(())
            })?)?;
        }

        // nexus._bind(keys, action)
        {
            let cfg2 = cfg.clone();
            nexus.set("_bind", self.lua.create_function(move |_, (keys, action): (String, String)| {
                cfg2.write().keybindings.push(KeyBinding { keys, action });
                Ok(())
            })?)?;
        }

        // nexus._log(level, msg)
        nexus.set("_log", self.lua.create_function(|_, (level, msg): (String, String)| {
            match level.as_str() {
                "INFO"  => log::info!("[lua] {}", msg),
                "WARN"  => log::warn!("[lua] {}", msg),
                "ERROR" => log::error!("[lua] {}", msg),
                _       => log::debug!("[lua] {}", msg),
            }
            Ok(())
        })?)?;

        self.lua.globals().set("_nexus_api", nexus)?;
        Ok(())
    }

    /// Load the built-in Lua standard library.
    fn load_stdlib(&self) -> Result<()> {
        // Embed the api.lua source directly so we don't need a file at runtime.
        let stdlib = include_str!("../plugins/api.lua");
        self.lua.load(stdlib)
            .set_name("@api.lua")
            .exec()
            .context("Failed to load Lua stdlib")?;
        Ok(())
    }

    /// Load user's ~/.config/nexus/init.lua. Silently skips if missing.
    pub fn load_user_config(&self) -> Result<()> {
        if !self.config_path.exists() {
            log::info!("[config] No init.lua found, using defaults");
            return Ok(());
        }
        let src = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("Cannot read {:?}", self.config_path))?;
        self.lua.load(&src)
            .set_name(format!("@{}", self.config_path.display()))
            .exec()
            .with_context(|| "Error in init.lua")?;
        log::info!("[config] Loaded {:?}", self.config_path);
        Ok(())
    }

    /// Hot-reload: re-execute init.lua without restarting.
    pub fn reload(&self) -> Result<()> {
        self.load_user_config()
    }

    /// Get a snapshot of the current config.
    pub fn config(&self) -> CompositorConfig {
        self.config.read().clone()
    }
}
