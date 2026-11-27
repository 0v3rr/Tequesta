//! Runtime & restart settings persisted in `tequesta_config.toml` and
//! edited live via the native `F1` overlay menu.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TequestaConfig {
    /// Static command-list bypass & cache (`VICTORY_PORT_STATIC`).
    pub port_static: bool,
    /// Live FPS cap (`0` = unlimited, `30`, `60`).
    pub fps_cap: u32,
    /// Mouse look sensitivity multiplier (`0.25..=3.0`).
    pub mouse_sensitivity: f32,
    /// Compact FPS/frametime corner counter (`true` by default).
    pub show_fps_hud: bool,
    /// Internal display resolution scale (`50`, `75`, `100` percent of 1080p).
    pub resolution_scale: u32,
    /// Borderless fullscreen window (`true`) vs windowed (`false`).
    pub borderless_fullscreen: bool,
    /// Shader raw views (`VICTORY_RAW_VIEWS=1`).
    pub raw_views: bool,
    /// Native Raw Input keyboard/mouse (`VICTORY_KBM_NATIVE=1`).
    pub kbm_native: bool,
}

impl Default for TequestaConfig {
    fn default() -> Self {
        Self {
            port_static: true,
            fps_cap: 0,
            mouse_sensitivity: 1.0,
            show_fps_hud: true,
            resolution_scale: 100,
            borderless_fullscreen: false,
            raw_views: false,
            kbm_native: true,
        }
    }
}

fn env_flag_set(name: &str) -> Option<bool> {
    let val = std::env::var(name).ok()?;
    Some(matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

impl TequestaConfig {
    /// Path to `tequesta_config.toml` in the current working directory
    /// (falling back beside the executable).
    pub fn path() -> PathBuf {
        if let Ok(cwd) = std::env::current_dir() {
            return cwd.join("tequesta_config.toml");
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tequesta_config.toml")))
            .unwrap_or_else(|| PathBuf::from("tequesta_config.toml"))
    }

    /// Loads `tequesta_config.toml` if present; otherwise initializes defaults
    /// from environment variables.
    pub fn load() -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(mut cfg) = toml::from_str::<Self>(&text)
        {
            cfg.sanitize();
            return cfg;
        }
        Self::from_env()
    }

    /// Builds a default configuration seeded from current environment variables.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Some(v) = env_flag_set("VICTORY_PORT_STATIC") {
            cfg.port_static = v;
        }
        if let Some(v) = env_flag_set("VICTORY_RAW_VIEWS") {
            cfg.raw_views = v;
        }
        if let Some(v) = env_flag_set("VICTORY_KBM_NATIVE") {
            cfg.kbm_native = v;
        } else if let Some(v) = env_flag_set("VICTORY_KBM_GTA") {
            cfg.kbm_native = v;
        }
        if let Some(v) = env_flag_set("VICTORY_BORDERLESS") {
            cfg.borderless_fullscreen = v;
        }
        if let Ok(s) = std::env::var("VICTORY_MOUSE_SENS")
            && let Ok(v) = s.trim().parse::<f32>()
            && v.is_finite()
        {
            cfg.mouse_sensitivity = v;
        }
        cfg.sanitize();
        cfg
    }

    pub fn sanitize(&mut self) {
        self.fps_cap = match self.fps_cap {
            30 | 60 => self.fps_cap,
            _ => 0,
        };
        if !self.mouse_sensitivity.is_finite() {
            self.mouse_sensitivity = 1.0;
        }
        self.mouse_sensitivity = self.mouse_sensitivity.clamp(0.25, 3.0);
        self.resolution_scale = match self.resolution_scale {
            50 | 75 | 100 => self.resolution_scale,
            _ => 100,
        };
    }

    /// Persists the configuration to `tequesta_config.toml`.
    pub fn save(&self) -> std::io::Result<()> {
        let mut clean = self.clone();
        clean.sanitize();
        let body = toml::to_string_pretty(&clean).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), body)
    }

    /// Applies restart-scoped settings to the process environment at startup
    /// before subsystems initialize.
    pub fn apply_startup_env(&self) {
        // SAFETY: called single-threaded at process startup before spawning workers.
        unsafe {
            std::env::set_var("VICTORY_PORT_STATIC", if self.port_static { "1" } else { "0" });
            std::env::set_var("VICTORY_RAW_VIEWS", if self.raw_views { "1" } else { "0" });
            std::env::set_var("VICTORY_KBM_NATIVE", if self.kbm_native { "1" } else { "0" });
            if self.kbm_native {
                std::env::set_var("VICTORY_KBM_GTA", "1");
            }
            std::env::set_var(
                "VICTORY_BORDERLESS",
                if self.borderless_fullscreen { "1" } else { "0" },
            );
            std::env::set_var("VICTORY_MOUSE_SENS", format!("{:.2}", self.mouse_sensitivity));
            if self.resolution_scale != 100 && std::env::var_os("VICTORY_DISPLAY_MODE").is_none() {
                let mode = match self.resolution_scale {
                    50 => "960x540",
                    75 => "1440x810",
                    _ => "1920x1080",
                };
                std::env::set_var("VICTORY_DISPLAY_MODE", mode);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip_and_sanitize() {
        let mut cfg = TequestaConfig {
            port_static: false,
            fps_cap: 45, // invalid -> 0
            mouse_sensitivity: 1.75,
            show_fps_hud: false,
            resolution_scale: 75,
            borderless_fullscreen: true,
            raw_views: true,
            kbm_native: false,
        };
        cfg.sanitize();
        assert_eq!(cfg.fps_cap, 0);
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: TequestaConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);
    }
}
