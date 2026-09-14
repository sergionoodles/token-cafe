//! Codex probe: `codex app-server` rate limits + usage, reset forecast.

use crate::rpc::JsonLineProcess;
use crate::util::{clean_error, executable_command, iso_timestamp, num, remaining, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn win<'a>(snap: &'a Value, key: &str) -> Option<&'a Value> {
    snap.get(key).filter(|v| v.is_object())
}

/// Canonical 5h window (300 min), else largest sub-weekly window.
pub fn select_five_hour(snapshot: &Value) -> Option<Value> {
    let windows: Vec<&Value> = ["primary", "secondary"].iter().filter_map(|k| win(snapshot, k)).collect();
    if let Some(w) = windows
        .iter()
        .find(|w| num(w.get("windowDurationMins"), 0.0) == 300.0)
    {
        return Some((*w).clone());
    }
    windows
        .into_iter()
        .filter(|w| {
            let d = num(w.get("windowDurationMins"), 0.0);
            d > 0.0 && d < 7.0 * 24.0 * 60.0
        })
        .max_by(|a, b| {
            num(a.get("windowDurationMins"), 0.0)
                .partial_cmp(&num(b.get("windowDurationMins"), 0.0))
                .unwrap()
        })
        .cloned()
}

/// Largest window >= 7 days.
pub fn select_weekly(snapshot: &Value) -> Option<Value> {
    ["primary", "secondary"]
        .iter()
        .filter_map(|k| win(snapshot, k))
        .filter(|w| num(w.get("windowDurationMins"), 0.0) >= 7.0 * 24.0 * 60.0)
        .max_by(|a, b| {
            num(a.get("windowDurationMins"), 0.0)
                .partial_cmp(&num(b.get("windowDurationMins"), 0.0))
                .unwrap()
        })
        .cloned()
}

fn reset_forecast(timeout: Duration) -> i64 {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout.min(Duration::from_secs(10)))
        .user_agent("TokenCafe")
        .build();
    let client = match client {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let resp = client
        .get("https://www.willcodexquotareset.com/api/forecast")
        .header("Accept", "application/json")
        .send();
    let score = resp
        .ok()
        .and_then(|r| r.json::<Value>().ok())
        .as_ref()
        .and_then(|p| p.get("forecast"))
        .and_then(|f| f.get("score"))
        .map(|s| num(Some(s), -1.0))
        .unwrap_or(-1.0);
    if score >= 0.0 {
        score.clamp(0.0, 100.0).round() as i64
    } else {
        -1
    }
}

pub fn probe(cli_bin: &str, deadline: Instant) -> Value {
    match probe_inner(cli_bin, deadline) {
        Ok(v) => v,
        Err(e) => {
            let mut v = base();
            v["usageStatusText"] = Value::from(clean_error(e.0));
            v
        }
    }
}

fn base() -> Value {
    json!({
        "provider": "codex",
        "ready": false,
        "usageStatusText": "",
        "rateLimitPercent": -1,
        "rateLimitLabel": "5-hour",
        "rateLimitResetAt": "",
        "secondaryRateLimitPercent": -1,
        "secondaryRateLimitLabel": "Weekly (7-day)",
        "secondaryRateLimitResetAt": "",
        "todayTotalTokens": 0,
        "dailyUsageBuckets": [],
        "lifetimeTokens": 0,
        "resetCreditsAvailable": -1,
        "resetForecastPercent": -1,
        "tierLabel": "",
        "balanceLabel": "",
        "balanceValue": "",
    })
}

fn probe_inner(cli_bin: &str, deadline: Instant) -> Result<Value> {
    let mut argv = executable_command(cli_bin)?;
    argv.extend(["app-server".to_string(), "--stdio".to_string()]);
    let mut rpc = JsonLineProcess::spawn(&argv)?;
    rpc.request(
        1,
        "initialize",
        Some(json!({
            "clientInfo": {"name": "token-cafe", "title": "Token Cafe", "version": "1"},
            "capabilities": {"experimentalApi": true},
        })),
        deadline,
    )?;
    rpc.send(&json!({"jsonrpc": "2.0", "method": "initialized"}))?;
    let limits = rpc.request(2, "account/rateLimits/read", None, deadline)?;
    let usage = rpc.request(3, "account/usage/read", None, deadline)?;

    let mut out = base();
    let empty = Value::Null;
    let snapshots = limits.get("rateLimitsByLimitId").unwrap_or(&empty);
    let codex_snap = snapshots.get("codex").unwrap_or(&empty);
    let snapshot = if codex_snap.is_object() {
        codex_snap
    } else {
        limits.get("rateLimits").unwrap_or(&empty)
    };

    let five = select_five_hour(snapshot).unwrap_or(Value::Null);
    let weekly = select_weekly(snapshot).unwrap_or(Value::Null);
    let five_used = num(five.get("usedPercent"), -1.0);
    if five_used >= 0.0 {
        out["rateLimitPercent"] = json!((five_used / 100.0).clamp(0.0, 1.0));
        out["rateLimitLabel"] = Value::from("5-hour");
    }
    out["rateLimitResetAt"] = Value::from(iso_timestamp(five.get("resetsAt")));
    let weekly_used = num(weekly.get("usedPercent"), -1.0);
    if weekly_used >= 0.0 {
        out["secondaryRateLimitPercent"] = json!((weekly_used / 100.0).clamp(0.0, 1.0));
        out["secondaryRateLimitLabel"] = Value::from("Weekly (7-day)");
    }
    out["secondaryRateLimitResetAt"] = Value::from(iso_timestamp(weekly.get("resetsAt")));
    out["tierLabel"] = snapshot
        .get("planType")
        .and_then(|v| v.as_str())
        .map(Value::from)
        .unwrap_or(Value::from(""));

    let reset_credits = limits.get("rateLimitResetCredits").unwrap_or(&empty);
    let reset_count = num(reset_credits.get("availableCount"), -1.0) as i64;
    if reset_count >= 0 {
        out["resetCreditsAvailable"] = json!(reset_count);
        out["balanceLabel"] = Value::from("Reset credits");
        out["balanceValue"] = Value::from(reset_count.to_string());
    }

    // Today is computed in the *local* timezone (YYYY-MM-DD compare).
    let today = local_day_string();
    if let Some(buckets) = usage.get("dailyUsageBuckets").and_then(|v| v.as_array()) {
        let mut norm = Vec::new();
        for b in buckets {
            let date = b
                .get("startDate")
                .and_then(|v| v.as_str())
                .map(|s| s[..10.min(s.len())].to_string())
                .unwrap_or_default();
            let tokens = num(b.get("tokens"), 0.0).max(0.0) as i64;
            if !date.is_empty() {
                norm.push(json!({"date": date, "tokens": tokens}));
            }
            if date == today {
                out["todayTotalTokens"] = json!(tokens);
            }
        }
        out["dailyUsageBuckets"] = Value::from(norm);
    }
    let summary = usage.get("summary").unwrap_or(&empty);
    let lifetime = num(summary.get("lifetimeTokens"), 0.0).max(0.0) as i64;
    out["lifetimeTokens"] = json!(lifetime);
    if reset_count < 0 {
        out["balanceLabel"] = Value::from("Lifetime tokens");
        out["balanceValue"] = Value::from(if lifetime > 0 { lifetime.to_string() } else { String::new() });
    }
    out["resetForecastPercent"] = json!(reset_forecast(remaining(deadline).unwrap_or(Duration::ZERO)));
    out["ready"] = Value::from(true);
    out["usageStatusText"] = Value::from("");
    Ok(out)
}

fn local_day_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Local midnight via libc (DST-correct, unlike `% 86400`).
    unsafe {
        let t = now as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_seven_day_primary_window() {
        let snap = json!({"primary": {"usedPercent": 50, "windowDurationMins": 10080}, "secondary": null});
        assert_eq!(select_weekly(&snap).unwrap()["usedPercent"], json!(50));
    }

    #[test]
    fn restores_five_hour_window() {
        let snap = json!({"primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1234567890000i64}});
        assert_eq!(select_five_hour(&snap).unwrap()["usedPercent"], json!(25));
        assert!(select_weekly(&snap).is_none());
    }

    #[test]
    fn selects_both_windows_when_present() {
        let snap = json!({
            "primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1000},
            "secondary": {"usedPercent": 60, "windowDurationMins": 10080, "resetsAt": 2000},
        });
        assert_eq!(select_five_hour(&snap).unwrap()["usedPercent"], json!(25));
        assert_eq!(select_weekly(&snap).unwrap()["usedPercent"], json!(60));
    }
}
