//! Application state: config, overrides, HTTP client, usage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::config::{Config, OverrideEntry};
use crate::usage::Usage;

pub struct AppState {
    pub cfg: RwLock<Config>,
    pub overrides: RwLock<HashMap<String, OverrideEntry>>,
    pub config_path: PathBuf,
    pub overrides_path: PathBuf,
    cfg_mtime: Mutex<Option<SystemTime>>,
    ov_mtime: Mutex<Option<SystemTime>>,
    pub client: reqwest::Client,
    pub usage: Usage,
}

pub fn load_overrides(path: &Path) -> HashMap<String, OverrideEntry> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn save_overrides(
    path: &Path,
    ov: &HashMap<String, OverrideEntry>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let s = serde_json::to_string_pretty(ov).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, s).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

impl AppState {
    pub async fn new(config_path: PathBuf) -> Result<Self, String> {
        let cfg = Config::load(&config_path)?;
        let overrides_path = cfg
            .server
            .overrides_path
            .clone()
            .unwrap_or_else(|| {
                config_path
                    .parent()
                    .map(|p| p.join("overrides.json"))
                    .unwrap_or_else(|| PathBuf::from("overrides.json"))
            });
        let overrides = load_overrides(&overrides_path);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .read_timeout(std::time::Duration::from_secs(300))
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|e| e.to_string())?;
        let usage = Usage::new(cfg.server.usage_log.clone());
        let cfg_mtime = std::fs::metadata(&config_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let ov_mtime = std::fs::metadata(&overrides_path)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(Self {
            cfg: RwLock::new(cfg),
            overrides: RwLock::new(overrides),
            config_path,
            overrides_path,
            cfg_mtime: Mutex::new(cfg_mtime),
            ov_mtime: Mutex::new(ov_mtime),
            client,
            usage,
        })
    }

    /// Reload the config file and the overrides file when either changed
    /// on disk. Cheap: one `stat` per file. A broken config is logged and
    /// the previous one is kept.
    pub async fn maybe_reload(&self) {
        if let Ok(m) = std::fs::metadata(&self.config_path).and_then(|m| m.modified()) {
            if self
                .cfg_mtime
                .lock()
                .unwrap()
                .map_or(true, |last| m > last)
            {
                match Config::load(&self.config_path) {
                    Ok(cfg) => {
                        tracing::info!(
                            path = %self.config_path.display(),
                            "config reloaded"
                        );
                        *self.cfg.write().await = cfg;
                        *self.cfg_mtime.lock().unwrap() = Some(m);
                    }
                    Err(e) => {
                        tracing::error!("config reload failed, keeping old config: {e}")
                    }
                }
            }
        }
        if let Ok(m) = std::fs::metadata(&self.overrides_path).and_then(|m| m.modified()) {
            if self
                .ov_mtime
                .lock()
                .unwrap()
                .map_or(true, |last| m > last)
            {
                let ov = load_overrides(&self.overrides_path);
                *self.overrides.write().await = ov;
                *self.ov_mtime.lock().unwrap() = Some(m);
            }
        }
    }

    /// Force a full reload (admin endpoint). Fails on an invalid config.
    pub async fn force_reload(&self) -> Result<String, String> {
        let cfg = Config::load(&self.config_path)?;
        *self.cfg.write().await = cfg;
        if let Ok(m) = std::fs::metadata(&self.config_path).and_then(|m| m.modified()) {
            *self.cfg_mtime.lock().unwrap() = Some(m);
        }
        let ov = load_overrides(&self.overrides_path);
        *self.overrides.write().await = ov;
        if let Ok(m) = std::fs::metadata(&self.overrides_path).and_then(|m| m.modified()) {
            *self.ov_mtime.lock().unwrap() = Some(m);
        }
        Ok("reloaded".into())
    }
}
