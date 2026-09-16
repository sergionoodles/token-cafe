//! Claude Code probe: token history from local transcript JSONLs.
//!
//! `~/.claude/projects/**/*.jsonl` holds one assistant message per turn
//! with `message.usage` token counts (input/output/cache). Summing those
//! per local day yields a Codex-style `recentDays` token chart plus
//! today's per-model breakdown. Quotas still come from the OAuth endpoint
//! in `lib/claude.luau`; this probe only fills usage history.

use crate::util::{clean_error, epoch_to_local_day, expand_home, local_day_string, parse_iso_to_epoch_secs, remaining, ProbeError, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

pub const HISTORY_DAYS: i64 = 15;
const MAX_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

fn i64_field(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(n.as_f64().unwrap_or(0.0).max(0.0) as i64).max(0),
        _ => 0,
    }
}

/// Total tokens for one `message.usage` object.
/// Top-level `cache_creation_input_tokens` already includes the ephemeral
/// sub-fields, so prefer it; fall back to summing the nested
/// `cache_creation` object when the flat field is absent (older CLIs).
pub fn usage_tokens(usage: &Value) -> i64 {
    let input = i64_field(usage.get("input_tokens"));
    let output = i64_field(usage.get("output_tokens"));
    let cache_read = i64_field(usage.get("cache_read_input_tokens"));
    let mut cache_create = i64_field(usage.get("cache_creation_input_tokens"));
    if cache_create == 0 {
        if let Some(obj) = usage.get("cache_creation").and_then(|v| v.as_object()) {
            let mut sum = 0i64;
            for v in obj.values() {
                if let Some(n) = v.as_i64() {
                    sum += n.max(0);
                } else if let Some(f) = v.as_f64() {
                    sum += f.max(0.0) as i64;
                }
            }
            cache_create = sum;
        }
    }
    (input + output + cache_read + cache_create).max(0)
}

fn collect_project_files(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_FILES {
            return;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_FILES {
                return;
            }
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if !p.is_symlink() {
                    stack.push(p);
                }
            } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
}

fn base() -> Value {
    json!({
        "provider": "claude",
        "ready": false,
        "usageStatusText": "",
        "todayTotalTokens": 0,
        "todayTokensByModel": {},
        "recentDays": [],
        "lastUsedAt": 0,
    })
}

pub fn probe(projects_dir: &str, deadline: Instant) -> Value {
    match probe_inner(projects_dir, deadline) {
        Ok(v) => v,
        Err(e) => {
            let mut v = base();
            v["usageStatusText"] = Value::from(clean_error(e.0));
            v
        }
    }
}

fn probe_inner(projects_dir: &str, deadline: Instant) -> Result<Value> {
    let mut out = base();
    let dir = if projects_dir.trim().is_empty() {
        expand_home("~/.claude/projects")
    } else if projects_dir.starts_with('~') {
        expand_home(projects_dir)
    } else {
        projects_dir.to_string()
    };
    let root = std::path::PathBuf::from(&dir);
    if !root.is_dir() {
        return Err(ProbeError(format!("Claude projects dir not found: {dir}")));
    }
    let mut files = Vec::new();
    collect_project_files(&root, &mut files);

    let today = local_day_string(0);
    let mut daily: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut today_models: HashMap<String, i64> = HashMap::new();
    let mut last_used: i64 = 0;

    for path in files {
        // Time-box: bail out gracefully, keeping what was tallied so far.
        if remaining(deadline).is_err() {
            break;
        }
        let md = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.len() > MAX_FILE_BYTES {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let msg = match v.get("message") {
                Some(m) => m,
                None => continue,
            };
            let usage = match msg.get("usage") {
                Some(u) => u,
                None => continue,
            };
            let toks = usage_tokens(usage);
            if toks <= 0 {
                continue;
            }
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(parse_iso_to_epoch_secs)
                .unwrap_or(0);
            if ts <= 0 {
                continue;
            }
            if ts > last_used {
                last_used = ts;
            }
            let day = epoch_to_local_day(ts);
            let e = daily.entry(day.clone()).or_insert((0, 0));
            e.0 += toks;
            e.1 += 1;
            if day == today {
                let model = msg
                    .get("model")
                    .and_then(|m| m.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                *today_models.entry(model).or_insert(0) += toks;
            }
        }
    }

    let mut recent = Vec::new();
    let mut today_total = 0i64;
    for offset in 0..HISTORY_DAYS {
        let date = local_day_string(offset - (HISTORY_DAYS - 1));
        let (toks, msgs) = daily.get(&date).copied().unwrap_or((0, 0));
        if offset == HISTORY_DAYS - 1 {
            today_total = toks;
        }
        recent.push(json!({"date": date, "tokens": toks, "messageCount": msgs}));
    }
    out["recentDays"] = Value::from(recent);
    out["todayTotalTokens"] = json!(today_total);
    let mut models = serde_json::Map::new();
    for (k, v) in today_models {
        models.insert(k, json!(v));
    }
    out["todayTokensByModel"] = Value::from(models);
    if last_used > 0 {
        out["lastUsedAt"] = json!(last_used);
    }
    out["ready"] = Value::from(true);
    out["usageStatusText"] = Value::from("");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_flat_usage_fields() {
        let u = json!({
            "input_tokens": 2,
            "output_tokens": 46,
            "cache_creation_input_tokens": 29899,
            "cache_read_input_tokens": 100,
        });
        assert_eq!(usage_tokens(&u), 2 + 46 + 29899 + 100);
    }

    #[test]
    fn falls_back_to_nested_cache_creation() {
        let u = json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation": {"ephemeral_1h_input_tokens": 100, "ephemeral_5m_input_tokens": 20},
            "cache_read_input_tokens": 7,
        });
        assert_eq!(usage_tokens(&u), 10 + 5 + 120 + 7);
    }

    #[test]
    fn ignores_empty_usage() {
        assert_eq!(usage_tokens(&json!({})), 0);
    }

    #[test]
    fn missing_dir_errors() {
        let d = Instant::now() + std::time::Duration::from_secs(5);
        assert!(probe_inner("/definitely/missing/tc-claude-test", d).is_err());
    }

    #[test]
    fn tallies_transcript_fixture() {
        let base = std::env::temp_dir().join(format!("tc-claude-test-{}", std::process::id()));
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let today = local_day_string(0);
        let line = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}T12:00:00Z\",\"message\":{{\"model\":\"claude-opus-5\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":20,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":40}}}}}}",
            today
        );
        std::fs::write(proj.join("s1.jsonl"), line + "\n{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();
        let d = Instant::now() + std::time::Duration::from_secs(5);
        let out = probe_inner(base.to_str().unwrap(), d).unwrap();
        assert_eq!(out["todayTotalTokens"], json!(100));
        assert_eq!(out["todayTokensByModel"]["claude-opus-5"], json!(100));
        let recent = out["recentDays"].as_array().unwrap();
        assert_eq!(recent.len(), HISTORY_DAYS as usize);
        assert_eq!(recent.last().unwrap()["tokens"], json!(100));
        let _ = std::fs::remove_dir_all(&base);
    }
}
