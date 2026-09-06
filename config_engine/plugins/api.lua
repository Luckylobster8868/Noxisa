-- NexusOS Lua Configuration API
-- Loaded before user's ~/.config/nexus/init.lua
-- All nexus.* functions call back into Rust via _nexus_api.*

nexus = {}

-- ── Theme ──────────────────────────────────────────────────────────────────
local THEMES = {
    dark  = { bg="#0f1117", fg="#e2e8f0", accent="#7c3aed", border="#2d3748" },
    light = { bg="#ffffff", fg="#0f172a", accent="#6d28d9", border="#e2e8f0" },
    nord  = { bg="#2e3440", fg="#d8dee9", accent="#88c0d0", border="#4c566a" },
}

function nexus.theme(name, overrides)
    local t = THEMES[name] or THEMES.dark
    if overrides then
        for k, v in pairs(overrides) do t[k] = v end
    end
    _nexus_api._apply_theme(t)
end

-- ── Layout ─────────────────────────────────────────────────────────────────
function nexus.gap(px)           _nexus_api._set("gap",           px)  end
function nexus.border(w, r)      _nexus_api._set("border_width",  w or 1)
                                 _nexus_api._set("border_radius",  r or 8) end
function nexus.animations(on, s) _nexus_api._set("animations",    on ~= false)
                                 _nexus_api._set("anim_speed",    s or 1.0) end
function nexus.blur(on, r)       _nexus_api._set("blur",          on ~= false)
                                 _nexus_api._set("blur_radius",   r or 10) end
function nexus.layout(mode)      _nexus_api._set("layout",        mode)  end
function nexus.desktops(n)       _nexus_api._set("desktop_count", n)     end

-- ── Keybindings ────────────────────────────────────────────────────────────
function nexus.bind(keys, action)
    if type(action) == "function" then
        -- Store Lua callbacks in a registry
        nexus._callbacks = nexus._callbacks or {}
        local id = "lua_cb_" .. keys
        nexus._callbacks[id] = action
        _nexus_api._bind(keys, id)
    else
        _nexus_api._bind(keys, action)
    end
end

-- ── Window rules ───────────────────────────────────────────────────────────
nexus._rules = {}
function nexus.rule(matcher, action)
    table.insert(nexus._rules, { matcher = matcher, action = action })
end

-- ── Logging ────────────────────────────────────────────────────────────────
nexus.log = {}
function nexus.log.info(m)  _nexus_api._log("INFO",  tostring(m)) end
function nexus.log.warn(m)  _nexus_api._log("WARN",  tostring(m)) end
function nexus.log.error(m) _nexus_api._log("ERROR", tostring(m)) end
function nexus.log.debug(m) _nexus_api._log("DEBUG", tostring(m)) end

-- ── Hot corners ────────────────────────────────────────────────────────────
function nexus.hot_corner(pos, action)
    local map = { topleft=0, topright=1, bottomleft=2, bottomright=3 }
    local idx = map[pos]
    if idx then _nexus_api._set("hot_corner_" .. idx, action) end
end

-- ── AI configuration ───────────────────────────────────────────────────────
nexus.ai = {}
function nexus.ai.set_key(k)        _nexus_api._set("ai_key",        k or "")  end
function nexus.ai.set_model(m)      _nexus_api._set("ai_model",      m)         end
function nexus.ai.prefer_offline(v) _nexus_api._set("ai_offline",    v ~= false) end
function nexus.ai.delay(ms)         _nexus_api._set("ai_delay_ms",   ms)        end

-- ── System ─────────────────────────────────────────────────────────────────
function nexus.reload()  _nexus_api._set("reload", true) end
function nexus.env(name) return os.getenv(name) end

-- ── Defaults (applied when user has no init.lua) ──────────────────────────
nexus.theme("dark")
nexus.gap(4)
nexus.border(1, 8)
nexus.animations(true, 1.0)
nexus.blur(true, 10)
nexus.layout("float")
nexus.desktops(4)

-- Default keybindings
nexus.bind("super+return",    "terminal")
nexus.bind("super+b",         "browser")
nexus.bind("super+q",         "close_window")
nexus.bind("super+f",         "toggle_fullscreen")
nexus.bind("super+space",     "launcher")
nexus.bind("super+tab",       "next_window")
nexus.bind("super+left",      "snap_left")
nexus.bind("super+right",     "snap_right")
nexus.bind("super+up",        "maximize")
nexus.bind("super+down",      "restore")
nexus.bind("super+1",         "desktop_1")
nexus.bind("super+2",         "desktop_2")
nexus.bind("super+3",         "desktop_3")
nexus.bind("super+4",         "desktop_4")
nexus.bind("super+shift+r",   function() nexus.reload() end)
nexus.bind("ctrl+alt+t",      "terminal")
nexus.bind("super+l",         "lock_screen")
nexus.bind("super+shift+s",   "screenshot")

-- Hot corners
nexus.hot_corner("topleft",     "overview")
nexus.hot_corner("bottomright", "launcher")
nexus.hot_corner("bottomleft",  "desktop")

nexus.log.info("NexusOS config API loaded")
