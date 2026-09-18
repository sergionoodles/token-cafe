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

/// OAuth token refresh for Claude Code (`tc-probe claude-auth`).
///
/// Overnight the access token in `~/.claude/.credentials.json` expires
/// (roughly every 8h) and the usage endpoint starts returning 401, which
/// surfaced as a morning "Token expired". Opening `claude` in a terminal
/// fixes it because the CLI performs a standard OAuth refresh-token grant
/// on startup — this probe does exactly that grant, with the CLI's public
/// client id and scopes, and writes the renewed tokens back to the
/// credentials file. The Luau side re-reads the file afterwards.
///
/// A `claude auth status` subprocess is deliberately NOT used: it reports
/// cached login state without renewing the access token.
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Public Claude Code OAuth client id (shipped in the CLI binary).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Default scopes requested by the CLI when none are stored.
const DEFAULT_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn auth_base() -> Value {
    json!({
        "provider": "claude-auth",
        "ready": false,
        "loggedIn": false,
        "refreshed": false,
        "subscriptionType": "",
        "usageStatusText": "",
    })
}

pub fn probe_auth(creds_path: &str, deadline: Instant) -> Value {
    match probe_auth_inner(creds_path, deadline) {
        Ok(v) => v,
        Err(e) => {
            let mut v = auth_base();
            v["usageStatusText"] = Value::from(clean_error(e.0));
            v
        }
    }
}

fn scope_string(oauth: &Value) -> String {
    if let Some(arr) = oauth.get("scopes").and_then(|s| s.as_array()) {
        let scopes: Vec<String> = arr
            .iter()
            .filter_map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !scopes.is_empty() {
            return scopes.join(" ");
        }
    }
    DEFAULT_SCOPES.join(" ")
}

fn probe_auth_inner(creds_path: &str, deadline: Instant) -> Result<Value> {
    let path = if creds_path.trim().is_empty() {
        expand_home("~/.claude/.credentials.json")
    } else if creds_path.starts_with('~') {
        expand_home(creds_path)
    } else {
        creds_path.to_string()
    };
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| ProbeError(format!("Credentials not found: {path}")))?;
    let mut data: Value =
        serde_json::from_str(&raw).map_err(|e| ProbeError(format!("Credentials file is not valid JSON: {e}")))?;
    let oauth = data
        .get("claudeAiOauth")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| ProbeError("Not logged in".to_string()))?;
    let now = now_ms();
    let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
    let sub = oauth.get("subscriptionType").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // Token still valid: nothing to do, no network involved.
    if !access.is_empty() && !(expires_at > 0 && expires_at <= now) {
        let mut v = auth_base();
        v["ready"] = Value::from(true);
        v["loggedIn"] = Value::from(true);
        v["subscriptionType"] = Value::from(sub);
        return Ok(v);
    }

    let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap_or("");
    if refresh.is_empty() {
        let mut v = auth_base();
        v["subscriptionType"] = Value::from(sub);
        v["usageStatusText"] = Value::from("Not logged in");
        return Ok(v);
    }
    let refresh_exp = oauth.get("refreshTokenExpiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
    if refresh_exp > 0 && refresh_exp <= now {
        let mut v = auth_base();
        v["subscriptionType"] = Value::from(sub);
        v["usageStatusText"] = Value::from("Refresh token expired — log in again");
        return Ok(v);
    }

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": CLIENT_ID,
        "scope": scope_string(&oauth),
    });
    let timeout = remaining(deadline)?.min(std::time::Duration::from_secs(30));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent("TokenCafe")
        .build()
        .map_err(|e| ProbeError(format!("Refresh request failed: {e}")))?;
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| ProbeError(format!("Refresh request failed: {e}")))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let detail: String = resp.text().unwrap_or_default().chars().take(200).collect();
        // The refresh token itself is dead: re-login is required, retrying
        // the same grant cannot succeed.
        if status == 400 || status == 401 {
            let mut v = auth_base();
            v["subscriptionType"] = Value::from(sub);
            v["usageStatusText"] = Value::from("Re-login required");
            if detail.contains("invalid_grant") {
                v["usageStatusText"] = Value::from("Re-login required");
            }
            return Ok(v);
        }
        return Err(ProbeError(format!("Token refresh failed (HTTP {status}): {detail}")));
    }
    let token: Value = resp
        .json()
        .map_err(|e| ProbeError(format!("Invalid refresh response: {e}")))?;
    let new_access = token.get("access_token").and_then(|v| v.as_str()).unwrap_or("");
    if new_access.is_empty() {
        return Err(ProbeError("Refresh response missing access token".to_string()));
    }
    let expires_in = token
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .or_else(|| token.get("expires_in").and_then(|v| v.as_f64()).map(|f| f as i64))
        .unwrap_or(0);
    if expires_in <= 0 {
        return Err(ProbeError("Refresh response missing expires_in".to_string()));
    }
    let new_refresh = token.get("refresh_token").and_then(|v| v.as_str()).unwrap_or(refresh);
    let oauth_mut = data.get_mut("claudeAiOauth").ok_or_else(|| ProbeError("Not logged in".to_string()))?;
    oauth_mut["accessToken"] = Value::from(new_access);
    oauth_mut["refreshToken"] = Value::from(new_refresh);
    oauth_mut["expiresAt"] = json!(now + expires_in * 1000);
    // `refresh_token_expires_in` is seconds from now when the server rotates
    // the refresh token; absent means the old expiry still holds.
    if let Some(rtei) = token
        .get("refresh_token_expires_in")
        .and_then(|v| v.as_i64())
        .or_else(|| token.get("refresh_token_expires_in").and_then(|v| v.as_f64()).map(|f| f as i64))
    {
        if rtei > 0 {
            oauth_mut["refreshTokenExpiresAt"] = json!(now + rtei * 1000);
        }
    }
    if let Some(scope) = token.get("scope").and_then(|v| v.as_str()) {
        let scopes: Vec<Value> = scope.split_whitespace().map(Value::from).collect();
        if !scopes.is_empty() {
            oauth_mut["scopes"] = Value::from(scopes);
        }
    }
    std::fs::write(&path, serde_json::to_string(&data).unwrap_or(raw))
        .map_err(|e| ProbeError(format!("Could not save refreshed credentials: {e}")))?;

    let mut v = auth_base();
    v["ready"] = Value::from(true);
    v["loggedIn"] = Value::from(true);
    v["refreshed"] = Value::from(true);
    v["subscriptionType"] = Value::from(sub);
    Ok(v)
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

    #[test]
    fn auth_reports_valid_token_without_network() {
        let dir = std::env::temp_dir().join(format!("tc-claude-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        let future = now_ms() + 8 * 3600 * 1000;
        std::fs::write(
            &path,
            format!(
                "{{\"claudeAiOauth\":{{\"accessToken\":\"at\",\"refreshToken\":\"rt\",\"expiresAt\":{future},\"subscriptionType\":\"pro\"}}}}"
            ),
        )
        .unwrap();
        let d = Instant::now() + std::time::Duration::from_secs(5);
        let out = probe_auth(path.to_str().unwrap(), d);
        assert_eq!(out["provider"], json!("claude-auth"));
        assert_eq!(out["ready"], json!(true));
        assert_eq!(out["loggedIn"], json!(true));
        assert_eq!(out["refreshed"], json!(false));
        assert_eq!(out["subscriptionType"], json!("pro"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_missing_file_errors() {
        let d = Instant::now() + std::time::Duration::from_secs(5);
        let out = probe_auth("/definitely/missing/tc-claude-auth-test.json", d);
        assert_eq!(out["provider"], json!("claude-auth"));
        assert_eq!(out["ready"], json!(false));
    }

    #[test]
    fn auth_expired_without_refresh_token_is_logged_out() {
        let dir = std::env::temp_dir().join(format!("tc-claude-auth-test-nort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        let past = now_ms() - 60_000;
        std::fs::write(
            &path,
            format!("{{\"claudeAiOauth\":{{\"accessToken\":\"at\",\"expiresAt\":{past}}}}}"),
        )
        .unwrap();
        let d = Instant::now() + std::time::Duration::from_secs(5);
        let out = probe_auth(path.to_str().unwrap(), d);
        assert_eq!(out["ready"], json!(false));
        assert_eq!(out["loggedIn"], json!(false));
        // No network attempted: file must be untouched.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"accessToken\":\"at\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_expired_refresh_token_is_logged_out() {
        let dir = std::env::temp_dir().join(format!("tc-claude-auth-test-rtexp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        let past = now_ms() - 60_000;
        std::fs::write(
            &path,
            format!(
                "{{\"claudeAiOauth\":{{\"accessToken\":\"at\",\"refreshToken\":\"rt\",\"expiresAt\":{past},\"refreshTokenExpiresAt\":{past}}}}}"
            ),
        )
        .unwrap();
        let d = Instant::now() + std::time::Duration::from_secs(5);
        let out = probe_auth(path.to_str().unwrap(), d);
        assert_eq!(out["ready"], json!(false));
        assert_eq!(out["loggedIn"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_string_prefers_stored_scopes() {
        let oauth = json!({"scopes": ["b", "a"]});
        assert_eq!(scope_string(&oauth), "b a");
        assert_eq!(scope_string(&json!({})), DEFAULT_SCOPES.join(" "));
    }
}
