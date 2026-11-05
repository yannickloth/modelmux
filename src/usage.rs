//! Per-request usage accounting: in-memory stats plus an optional JSONL log.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Record {
    pub alias: String,
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub stream: bool,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: u128,
    pub attempts: usize,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
struct Agg {
    requests: u64,
    errors: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cost: f64,
}

pub struct Usage {
    stats: Mutex<HashMap<(String, String), Agg>>,
    log_path: Option<PathBuf>,
}

impl Usage {
    pub fn new(log_path: Option<PathBuf>) -> Self {
        if let Some(p) = &log_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        Self {
            stats: Mutex::new(HashMap::new()),
            log_path,
        }
    }

    pub fn record(&self, r: &Record) {
        {
            let mut map = self.stats.lock().unwrap();
            let agg = map
                .entry((r.alias.clone(), r.provider.clone()))
                .or_default();
            agg.requests += 1;
            if r.status >= 400 || r.error.is_some() {
                agg.errors += 1;
            }
            agg.prompt_tokens += r.prompt_tokens.unwrap_or(0);
            agg.completion_tokens += r.completion_tokens.unwrap_or(0);
            agg.cost += r.cost_usd.unwrap_or(0.0);
        }
        if let Some(path) = &self.log_path {
            let line = json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "alias": r.alias,
                "provider": r.provider,
                "model": r.model,
                "status": r.status,
                "stream": r.stream,
                "prompt_tokens": r.prompt_tokens,
                "completion_tokens": r.completion_tokens,
                "cost_usd": r.cost_usd,
                "duration_ms": r.duration_ms,
                "attempts": r.attempts,
                "error": r.error,
            });
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let map = self.stats.lock().unwrap();
        let (mut requests, mut errors, mut pt, mut ct, mut cost) = (0u64, 0u64, 0u64, 0u64, 0.0f64);
        let mut rows: Vec<Value> = vec![];
        for ((alias, provider), a) in map.iter() {
            requests += a.requests;
            errors += a.errors;
            pt += a.prompt_tokens;
            ct += a.completion_tokens;
            cost += a.cost;
            rows.push(json!({
                "alias": alias,
                "provider": provider,
                "requests": a.requests,
                "errors": a.errors,
                "prompt_tokens": a.prompt_tokens,
                "completion_tokens": a.completion_tokens,
                "cost_usd": a.cost,
            }));
        }
        rows.sort_by(|a, b| {
            a["provider"]
                .as_str()
                .unwrap_or("")
                .cmp(b["provider"].as_str().unwrap_or(""))
        });
        json!({
            "requests": requests,
            "errors": errors,
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "cost_usd": cost,
            "by_alias_provider": rows,
        })
    }
}
