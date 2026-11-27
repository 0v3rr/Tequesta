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
    /// Mouse look sensitivity multiplier (`0.25..=8.0`).
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
    /// Indirect diffuse ambient stand-in (`VICTORY_AMBIENT`).
    pub ambient_gi: bool,
    /// Reflection & probe tile guard (`VICTORY_REFLECTION_GUARD`).
    pub reflection_probe_guard: bool,
    /// Parallel command list recording (`VICTORY_THIN=1`).
    pub thin_lists: bool,
    /// Command translation worker threads (`VICTORY_JOIN_PARTS`: `1`, `4`, `8`).
    pub join_parts: u32,
    /// Batched unified-memory card rehash (`VICTORY_CARD_REHASH_RUNS=1`).
    pub card_rehash_runs: bool,
    /// Precompile/preload shader translations at launch (`VICTORY_SHADER_PREWARM`).
    pub shader_prewarm: bool,
    /// Load and save compiled D3D12 PSOs in the on-disk pipeline library (`VICTORY_PIPELINE_PRECACHE`).
    pub pipeline_precache: bool,
}

impl Default for TequestaConfig {
    fn default() -> Self {
        Self {
            port_static: true,
            fps_cap: 0,
            mouse_sensitivity: 2.0,
            show_fps_hud: true,
            resolution_scale: 100,
            borderless_fullscreen: false,
            raw_views: false,
            kbm_native: true,
            ambient_gi: true,
            reflection_probe_guard: false,
            thin_lists: true,
            join_parts: 8,
            card_rehash_runs: true,
            shader_prewarm: true,
            pipeline_precache: true,
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
        if let Ok(v) = std::env::var("VICTORY_AMBIENT") {
            let trimmed = v.trim();
            cfg.ambient_gi = !trimmed.is_empty()
                && !matches!(trimmed.to_ascii_lowercase().as_str(), "0" | "off" | "false");
        }
        if let Some(v) = env_flag_set("VICTORY_REFLECTION_GUARD") {
            cfg.reflection_probe_guard = v;
        }
        if let Some(v) = env_flag_set("VICTORY_THIN") {
            cfg.thin_lists = v;
        }
        if let Ok(s) = std::env::var("VICTORY_JOIN_PARTS")
            && let Ok(v) = s.trim().parse::<u32>()
        {
            cfg.join_parts = v;
        }
        if let Some(v) = env_flag_set("VICTORY_CARD_REHASH_RUNS") {
            cfg.card_rehash_runs = v;
        }
        if let Some(v) = env_flag_set("VICTORY_SHADER_PREWARM") {
            cfg.shader_prewarm = v;
        }
        if let Some(v) = env_flag_set("VICTORY_PIPELINE_PRECACHE") {
            cfg.pipeline_precache = v;
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
            self.mouse_sensitivity = 2.0;
        }
        self.mouse_sensitivity = self.mouse_sensitivity.clamp(0.25, 8.0);
        self.resolution_scale = match self.resolution_scale {
            50 | 75 | 100 => self.resolution_scale,
            _ => 100,
        };
        self.join_parts = match self.join_parts {
            1 | 4 | 8 => self.join_parts,
            _ => 8,
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
            if self.ambient_gi {
                let unset_or_off = std::env::var("VICTORY_AMBIENT").is_err_and(|_| true)
                    || matches!(
                        std::env::var("VICTORY_AMBIENT")
                            .unwrap_or_default()
                            .trim()
                            .to_ascii_lowercase()
                            .as_str(),
                        "" | "0" | "off" | "false"
                    );
                if unset_or_off {
                    std::env::set_var("VICTORY_AMBIENT", "1.1");
                }
            } else {
                std::env::set_var("VICTORY_AMBIENT", "0");
            }
            std::env::set_var(
                "VICTORY_REFLECTION_GUARD",
                if self.reflection_probe_guard { "1" } else { "0" },
            );
            std::env::set_var("VICTORY_THIN", if self.thin_lists { "1" } else { "0" });
            std::env::set_var("VICTORY_JOIN_PARTS", self.join_parts.to_string());
            std::env::set_var(
                "VICTORY_CARD_REHASH_RUNS",
                if self.card_rehash_runs { "1" } else { "0" },
            );
            std::env::set_var(
                "VICTORY_SHADER_PREWARM",
                if self.shader_prewarm { "1" } else { "0" },
            );
            std::env::set_var(
                "VICTORY_PIPELINE_PRECACHE",
                if self.pipeline_precache { "1" } else { "0" },
            );
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
            mouse_sensitivity: 5.75,
            show_fps_hud: false,
            resolution_scale: 75,
            borderless_fullscreen: true,
            raw_views: true,
            kbm_native: false,
            ambient_gi: false,
            reflection_probe_guard: false,
            thin_lists: false,
            join_parts: 3, // invalid -> 8
            card_rehash_runs: false,
            shader_prewarm: false,
            pipeline_precache: false,
        };
        cfg.sanitize();
        assert_eq!(cfg.fps_cap, 0);
        assert_eq!(cfg.mouse_sensitivity, 5.75);
        assert_eq!(cfg.join_parts, 8);
        assert!(!cfg.shader_prewarm);
        assert!(!cfg.pipeline_precache);
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: TequestaConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);

        let mut high = TequestaConfig {
            mouse_sensitivity: 12.0,
            ..Default::default()
        };
        high.sanitize();
        assert_eq!(high.mouse_sensitivity, 8.0);
    }

    #[test]
    fn legacy_toml_defaults_prewarm_and_precache_to_true() {
        let legacy = r#"
port_static = true
fps_cap = 60
mouse_sensitivity = 2.5
show_fps_hud = true
resolution_scale = 75
borderless_fullscreen = true
raw_views = false
kbm_native = true
ambient_gi = true
reflection_probe_guard = false
thin_lists = true
join_parts = 4
card_rehash_runs = true
"#;
        let mut parsed: TequestaConfig = toml::from_str(legacy).unwrap();
        parsed.sanitize();
        assert!(parsed.shader_prewarm);
        assert!(parsed.pipeline_precache);
        assert_eq!(parsed.fps_cap, 60);
        assert_eq!(parsed.resolution_scale, 75);
        assert_eq!(parsed.join_parts, 4);
    }
}
