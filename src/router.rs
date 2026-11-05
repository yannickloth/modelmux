//! Resolves an inbound model alias to an ordered list of routing candidates.

use std::collections::HashMap;

use crate::config::{CandidateCfg, Config, OverrideEntry, ProviderCfg};
use crate::time_windows::window_active;

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub via_override: bool,
}

/// True when the candidate's window conditions allow it right now.
/// Each window is evaluated in its own timezone (window `tz`, else `tz`).
pub fn candidate_active(cfg: &Config, c: &CandidateCfg, tz: &str) -> bool {
    for name in &c.avoid_windows {
        if let Some(w) = cfg.windows.get(name) {
            if window_active(w, tz) {
                return false;
            }
        }
    }
    if !c.only_windows.is_empty() {
        let any = c
            .only_windows
            .iter()
            .any(|name| matches!(cfg.windows.get(name), Some(w) if window_active(w, tz)));
        if !any {
            return false;
        }
    }
    true
}

/// True when the provider's API key env var is set to a non-empty value.
pub fn key_available(p: &ProviderCfg) -> bool {
    match &p.api_key_env {
        Some(var) => match std::env::var(var) {
            Ok(v) => !v.trim().is_empty(),
            Err(_) => false,
        },
        None => true,
    }
}

/// Build the ordered candidate list for an alias.
///
/// Order: admin override (if any), then policy candidates that pass their
/// window conditions, skipping providers with missing keys. When nothing
/// else matches, the `default_provider` passthrough is added.
pub fn resolve(
    cfg: &Config,
    overrides: &HashMap<String, OverrideEntry>,
    alias: &str,
    tz: &str,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = vec![];

    if let Some(ov) = overrides.get(alias) {
        if cfg.providers.contains_key(&ov.provider) {
            out.push(Candidate {
                provider: ov.provider.clone(),
                model: ov.model.clone().unwrap_or_else(|| alias.to_string()),
                via_override: true,
            });
        }
    }

    if let Some(pol) = cfg.policies.get(alias) {
        for c in &pol.candidates {
            if !candidate_active(cfg, c, tz) {
                continue;
            }
            let Some(p) = cfg.providers.get(&c.provider) else {
                continue;
            };
            if !key_available(p) {
                tracing::debug!(
                    provider = %c.provider,
                    alias = %alias,
                    "skipping candidate: API key env var is missing or empty"
                );
                continue;
            }
            out.push(Candidate {
                provider: c.provider.clone(),
                model: c.model.clone(),
                via_override: false,
            });
        }
    }

    if out.is_empty() {
        if let Some(dp) = &cfg.server.default_provider {
            if let Some(p) = cfg.providers.get(dp) {
                if key_available(p) {
                    out.push(Candidate {
                        provider: dp.clone(),
                        model: alias.to_string(),
                        via_override: false,
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthStyle, PolicyCfg, ProviderKind};
    use std::collections::HashMap;

    fn provider(kind: ProviderKind) -> ProviderCfg {
        ProviderCfg {
            kind,
            base_url: "http://127.0.0.1:1".into(),
            api_key_env: None,
            price_windows: vec![],
            models: HashMap::new(),
            auth_style: AuthStyle::Bearer,
            anthropic_version: "2023-06-01".into(),
        }
    }

    fn cfg_with(windows: HashMap<String, crate::config::WindowCfg>, pol: PolicyCfg) -> Config {
        let mut providers: HashMap<String, ProviderCfg> = HashMap::new();
        providers.insert("a".into(), provider(ProviderKind::Openai));
        providers.insert("b".into(), provider(ProviderKind::Openai));
        Config {
            server: crate::config::ServerCfg {
                listen: "127.0.0.1:0".into(),
                api_key: None,
                usage_log: None,
                overrides_path: None,
                default_provider: None,
                default_max_tokens: 100,
                window_timezone: "utc".into(),
            },
            windows,
            providers,
            policies: {
                let mut m = HashMap::new();
                m.insert("alias".into(), pol);
                m
            },
        }
    }

    fn window(start: &str, end: &str) -> crate::config::WindowCfg {
        crate::config::WindowCfg {
            start: start.into(),
            end: end.into(),
            mult: 1.0,
            days: vec![],
            tz: None,
        }
    }

    #[test]
    fn avoid_window_skips_candidate() {
        // start == end is the full-day convention: the window is active at
        // every minute, so "a" is always avoided and "b" is the only
        // survivor regardless of the real clock.
        let w = window("00:00", "00:00");
        let mut windows = HashMap::new();
        windows.insert("all".into(), w);
        let cfg = cfg_with(
            windows.clone(),
            PolicyCfg {
                candidates: vec![
                    CandidateCfg {
                        provider: "a".into(),
                        model: "m".into(),
                        avoid_windows: vec!["all".into()],
                        only_windows: vec![],
                    },
                    CandidateCfg {
                        provider: "b".into(),
                        model: "m".into(),
                        avoid_windows: vec![],
                        only_windows: vec![],
                    },
                ],
            },
        );
        let cands = resolve(&cfg, &HashMap::new(), "alias", "utc");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "b");
    }

    #[test]
    fn override_comes_first_and_keeps_policy_as_fallback() {
        let cfg = cfg_with(
            HashMap::new(),
            PolicyCfg {
                candidates: vec![CandidateCfg {
                    provider: "a".into(),
                    model: "m".into(),
                    avoid_windows: vec![],
                    only_windows: vec![],
                }],
            },
        );
        let mut ov = HashMap::new();
        ov.insert(
            "alias".into(),
            OverrideEntry {
                provider: "b".into(),
                model: None,
            },
        );
        let cands = resolve(&cfg, &ov, "alias", "utc");
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].provider, "b");
        assert!(cands[0].via_override);
        assert_eq!(cands[0].model, "alias");
        assert_eq!(cands[1].provider, "a");
        assert!(!cands[1].via_override);
    }

    #[test]
    fn default_provider_for_unknown_alias() {
        let mut cfg = cfg_with(HashMap::new(), PolicyCfg { candidates: vec![] });
        cfg.server.default_provider = Some("a".into());
        // Empty-candidate policy is invalid in real configs, but resolve()
        // only reads; it must still fall through to the default provider.
        let cands = resolve(&cfg, &HashMap::new(), "some-other-model", "utc");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "a");
        assert_eq!(cands[0].model, "some-other-model");
    }

    #[test]
    fn only_windows_requires_activity() {
        // Build a full-day window whose day filter matches today's weekday
        // (deterministic against the clock) and one that excludes it.
        let wd = crate::time_windows::now_weekday_in("utc");
        let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let today = names[(wd - 1) as usize];
        let other = names[(wd as usize) % 7]; // different index

        let mut windows = HashMap::new();
        let w_match = crate::config::WindowCfg {
            start: "00:00".into(),
            end: "23:59".into(),
            mult: 1.0,
            days: vec![today.into()],
            tz: Some("utc".into()),
        };
        let w_other = crate::config::WindowCfg {
            start: "00:00".into(),
            end: "23:59".into(),
            mult: 1.0,
            days: vec![other.into()],
            tz: Some("utc".into()),
        };
        windows.insert("match".into(), w_match);
        windows.insert("other".into(), w_other);
        let cfg = cfg_with(
            windows,
            PolicyCfg {
                candidates: vec![
                    CandidateCfg {
                        provider: "a".into(),
                        model: "m".into(),
                        avoid_windows: vec!["other".into()],
                        only_windows: vec!["match".into()],
                    },
                    CandidateCfg {
                        provider: "a".into(),
                        model: "m2".into(),
                        avoid_windows: vec!["match".into()],
                        only_windows: vec![],
                    },
                ],
            },
        );
        // today's window is active, "other" is not: candidate 0 passes,
        // candidate 1 is avoided.
        assert!(candidate_active(
            &cfg,
            &cfg.policies["alias"].candidates[0],
            "utc"
        ));
        assert!(!candidate_active(
            &cfg,
            &cfg.policies["alias"].candidates[1],
            "utc"
        ));
    }
}
