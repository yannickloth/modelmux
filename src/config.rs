use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::time_windows::{is_day_name, is_valid_tz, parse_hhmm};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    #[serde(default)]
    pub windows: HashMap<String, WindowCfg>,
    pub providers: HashMap<String, ProviderCfg>,
    #[serde(default)]
    pub policies: HashMap<String, PolicyCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerCfg {
    /// Bind address, e.g. "127.0.0.1:8787".
    #[serde(default = "default_listen")]
    pub listen: String,
    /// If set, all requests must carry `Authorization: Bearer <api_key>`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Append-only JSONL log of every request (tokens, cost, latency).
    #[serde(default)]
    pub usage_log: Option<PathBuf>,
    /// Where the admin override endpoint persists overrides.
    /// Default: `overrides.json` next to the config file.
    #[serde(default)]
    pub overrides_path: Option<PathBuf>,
    /// Providers to try for model names that match no policy.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Sent to Anthropic-format providers when the client omits max_tokens.
    #[serde(default = "default_max_tokens")]
    pub default_max_tokens: u64,
    /// "utc" or "local". Windows are evaluated in this timezone.
    #[serde(default = "default_window_tz")]
    pub window_timezone: String,
}

fn default_listen() -> String {
    "127.0.0.1:8787".to_string()
}
fn default_max_tokens() -> u64 {
    8192
}
fn default_window_tz() -> String {
    "utc".to_string()
}

/// A named time range. `mult` is the price multiplier inside the window
/// (1.0 = no price effect). An empty `days` list means every day.
#[derive(Debug, Clone, Deserialize)]
pub struct WindowCfg {
    /// "HH:MM" (24h) in the window's timezone.
    pub start: String,
    pub end: String,
    #[serde(default = "default_mult", deserialize_with = "de_f64")]
    pub mult: f64,
    /// ["Mon".."Sun"]; full names accepted.
    #[serde(default)]
    pub days: Vec<String>,
    /// IANA timezone for this window (e.g. "Europe/Brussels" or "UTC").
    /// Defaults to `server.window_timezone`. Pin `tz = "UTC"` for schedules
    /// that are defined in UTC — local-time windows in a DST zone drift an
    /// hour twice a year.
    #[serde(default)]
    pub tz: Option<String>,
}

fn default_mult() -> f64 {
    1.0
}

/// Accept both TOML integers and floats for f64 fields.
fn de_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("number out of range")),
        _ => Err(serde::de::Error::custom("expected a number")),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCfg {
    /// "openai" (chat completions passthrough) or "anthropic"
    /// (/v1/messages, with request and response translation).
    pub kind: ProviderKind,
    /// Base URL without the trailing path.
    /// openai: `{base_url}/chat/completions`
    /// anthropic: `{base_url}/v1/messages`
    pub base_url: String,
    /// Environment variable that holds the API key.
    /// If missing or empty, candidates for this provider are skipped.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Window names during which prices are multiplied (cost accounting).
    #[serde(default)]
    pub price_windows: Vec<String>,
    /// Optional per-model prices, USD per 1M tokens (off-peak base).
    #[serde(default)]
    pub models: HashMap<String, ModelPrice>,
    /// "bearer" (default) or "api_key" (x-api-key header, Anthropic style).
    #[serde(default = "default_auth_style")]
    pub auth_style: AuthStyle,
    /// Value of the `anthropic-version` header.
    #[serde(default = "default_anthropic_version")]
    pub anthropic_version: String,
}

fn default_auth_style() -> AuthStyle {
    AuthStyle::Bearer
}
fn default_anthropic_version() -> String {
    "2023-06-01".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    Bearer,
    ApiKey,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPrice {
    #[serde(deserialize_with = "de_f64")]
    pub input_per_mtok: f64,
    #[serde(deserialize_with = "de_f64")]
    pub output_per_mtok: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyCfg {
    /// Ordered candidate list. The first candidate that passes its window
    /// conditions is tried; on failure the next is tried.
    #[serde(default)]
    pub candidates: Vec<CandidateCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CandidateCfg {
    pub provider: String,
    /// Model id to send to that provider.
    pub model: String,
    /// Skip this candidate while inside any of these windows.
    #[serde(default)]
    pub avoid_windows: Vec<String>,
    /// Only use this candidate while inside at least one of these windows.
    #[serde(default)]
    pub only_windows: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideEntry {
    pub provider: String,
    /// Model id for the provider. Defaults to the alias itself.
    #[serde(default)]
    pub model: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut errs: Vec<String> = vec![];
        for (name, w) in &self.windows {
            if parse_hhmm(&w.start).is_none() {
                errs.push(format!(
                    "window '{name}' has invalid start '{}', want HH:MM",
                    w.start
                ));
            }
            if parse_hhmm(&w.end).is_none() {
                errs.push(format!(
                    "window '{name}' has invalid end '{}', want HH:MM",
                    w.end
                ));
            }
            if let Some(tz) = &w.tz {
                if !is_valid_tz(tz) {
                    errs.push(format!(
                        "window '{name}' has unknown timezone '{tz}' \
                         (want \"utc\", \"local\", or an IANA name)"
                    ));
                }
            }
            for d in &w.days {
                if !is_day_name(d) {
                    errs.push(format!(
                        "window '{name}' has invalid day '{d}', want Mon..Sun"
                    ));
                }
            }
        }
        if !is_valid_tz(&self.server.window_timezone) {
            errs.push(format!(
                "server.window_timezone must be \"utc\", \"local\", or an IANA zone \
                 (e.g. \"Europe/Brussels\"), got '{}'",
                self.server.window_timezone
            ));
        }
        if let Some(dp) = &self.server.default_provider {
            if !self.providers.contains_key(dp) {
                errs.push(format!(
                    "server.default_provider '{dp}' is not in providers"
                ));
            }
        }
        for (name, p) in &self.providers {
            for w in &p.price_windows {
                if !self.windows.contains_key(w) {
                    errs.push(format!(
                        "provider '{name}' lists price window '{w}' that is not defined"
                    ));
                }
            }
        }
        for (alias, pol) in &self.policies {
            if pol.candidates.is_empty() {
                errs.push(format!("policy '{alias}' has no candidates"));
            }
            for c in &pol.candidates {
                if !self.providers.contains_key(&c.provider) {
                    errs.push(format!(
                        "policy '{alias}' references unknown provider '{}'",
                        c.provider
                    ));
                }
                for w in c.avoid_windows.iter().chain(c.only_windows.iter()) {
                    if !self.windows.contains_key(w) {
                        errs.push(format!("policy '{alias}' references unknown window '{w}'"));
                    }
                }
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }
}
