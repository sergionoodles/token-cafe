//! Grok probe: Grok Build CLI billing snapshot via `grok agent stdio`.
//!
//! The official binary is `grok` (installed from https://x.ai/cli —
//! "Grok Build"). Older releases exposed the billing extension as
//! `_x.ai/billing`; current releases use `x.ai/billing`. We try the new
//! name first and fall back to the legacy one so both work.

use crate::rpc::JsonLineProcess;
use crate::util::{clean_error, executable_command, iso_timestamp, num, ProbeError, Result};
use serde_json::{json, Value};
use std::time::Instant;

fn cent(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Object(m)) => Some(num(m.get("val"), 0.0) as i64),
        Some(Value::Null) | None => None,
        Some(other) => Some(num(Some(other), 0.0) as i64),
    }
}

fn unwrap_result(mut v: Value) -> Value {
    loop {
        match v {
            Value::Object(ref m) if m.len() == 1 && m.contains_key("result") => {
                v = m["result"].clone();
            }
            _ => return v,
        }
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
        "provider": "grok",
        "ready": false,
        "usageStatusText": "",
        "rateLimitPercent": -1,
        "rateLimitLabel": "Credits",
        "rateLimitResetAt": "",
        "tierLabel": "",
        "balanceLabel": "Credits",
        "balanceValue": "",
        "prepaidBalanceCents": null,
        "onDemandCapCents": null,
        "onDemandUsedCents": null,
        "resetCreditsAvailable": -1,
        "resetForecastPercent": -1,
    })
}

fn probe_inner(cli_bin: &str, deadline: Instant) -> Result<Value> {
    let mut out = base();
    let mut argv = executable_command(cli_bin)?;
    argv.extend(["agent".to_string(), "stdio".to_string()]);
    let mut rpc = JsonLineProcess::spawn(&argv)?;
    let init_result = rpc.request(
        1,
        "initialize",
        Some(json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "token-cafe", "title": "Token Cafe", "version": "1"},
            "_meta": {
                "startupHints": {"nonInteractive": true, "skipGitStatus": true, "skipProjectLayout": true},
                "clientType": "token-cafe",
                "clientVersion": "1",
            },
        })),
        deadline,
    )?;
    // Newer Grok Build CLIs gate extensions behind an explicit
    // `authenticate` step (see docs.x.ai/build/cli/headless-scripting).
    // Best-effort: use the cached login token when advertised, ignore
    // failures so old CLIs without this method keep working.
    try_authenticate(&mut rpc, &init_result, deadline);

    // Current Grok Build uses `x.ai/billing`; pre-Build releases used
    // `_x.ai/billing`. Try both so old + new CLIs work.
    let mut billing_err = String::new();
    let mut billing: Option<Value> = None;
    for method in ["x.ai/billing", "_x.ai/billing"] {
        match rpc.request(2, method, Some(json!({})), deadline) {
            Ok(v) => {
                billing = Some(unwrap_result(v));
                break;
            }
            Err(e) => {
                billing_err = e.0;
            }
        }
    }
    let billing = billing.ok_or_else(|| {
        // Surface auth hint verbatim when the CLI tells us to log in.
        if billing_err.to_lowercase().contains("auth") || billing_err.contains("login") {
            ProbeError(billing_err.clone())
        } else {
            ProbeError(format!("Grok billing unavailable ({billing_err})"))
        }
    })?;
    let config = extract_config(&billing)
        .ok_or_else(|| ProbeError("Grok billing data is unavailable".to_string()))?;

    apply_billing(&mut out, &billing, &config);
    out["ready"] = Value::from(true);
    out["usageStatusText"] = Value::from("");
    Ok(out)
}

/// Best-effort `authenticate` for Grok Build CLIs that advertise auth
/// methods in the `initialize` result. Old CLIs have no such method —
/// failures are silently ignored.
fn try_authenticate(rpc: &mut JsonLineProcess, init_result: &Value, deadline: Instant) {
    let methods = init_result
        .get("authMethods")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if methods.is_empty() {
        return;
    }
    let ids: Vec<String> = methods
        .iter()
        .filter_map(|m| {
            m.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    // Prefer a non-interactive cached login; fall back to API-key auth
    // when XAI_API_KEY is set (mirrors the official headless example).
    let has_api_key = std::env::var("XAI_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let pick = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|c| ids.iter().any(|id| id == **c))
            .map(|s| s.to_string())
    };
    let method_id = pick(&["cached_token", "cached-token", "groq.cached"])
        .or_else(|| {
            if has_api_key {
                pick(&["xai.api_key", "xai.api-key", "api_key", "apiKey"])
            } else {
                None
            }
        });
    if let Some(method_id) = method_id {
        let _ = rpc.request(
            3,
            "authenticate",
            Some(json!({"methodId": method_id, "_meta": {"headless": true}})),
            deadline,
        );
    }
}

/// The billing payload is `{config: {...}, subscriptionTier, ...}` on both
/// old and new CLIs, but accept a bare config object too (defensive).
fn extract_config(billing: &Value) -> Option<Value> {
    if let Some(cfg) = billing.get("config").filter(|v| v.is_object()) {
        return Some(cfg.clone());
    }
    // Bare shape (e.g. proxies returning GetGrokCreditsConfig directly).
    if billing.get("creditUsagePercent").is_some()
        || billing.get("monthlyLimit").is_some()
        || billing.get("currentPeriod").is_some()
    {
        return Some(billing.clone());
    }
    None
}

fn tier_of(billing: &Value, config: &Value) -> String {
    for key in ["subscriptionTier", "subscription_tier", "tier", "plan", "planType"] {
        if let Some(s) = billing.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            return s.to_string();
        }
        if let Some(s) = config.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            return s.to_string();
        }
    }
    String::new()
}

fn apply_billing(out: &mut Value, billing: &Value, config: &Value) {
    let mut usage_pct = num(config.get("creditUsagePercent"), -1.0);
    if usage_pct < 0.0 {
        let monthly = cent(config.get("monthlyLimit")).unwrap_or(0);
        let used = cent(config.get("used")).unwrap_or(0);
        if monthly > 0 {
            usage_pct = used as f64 / monthly as f64 * 100.0;
        }
    }
    if usage_pct >= 0.0 {
        out["rateLimitPercent"] = json!((usage_pct / 100.0).clamp(0.0, 1.0));
    }
    let period = config.get("currentPeriod").cloned().unwrap_or(Value::Null);
    let period_type = period
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    out["rateLimitLabel"] = Value::from(if period_type.contains("WEEK") {
        "Weekly credits"
    } else {
        "Monthly credits"
    });
    let reset_src = period
        .get("end")
        .or_else(|| config.get("billingPeriodEnd"))
        .cloned();
    out["rateLimitResetAt"] = Value::from(iso_timestamp(reset_src.as_ref()));
    let tier = tier_of(billing, config);
    out["tierLabel"] = Value::from(tier);

    let prepaid = cent(config.get("prepaidBalance"));
    let cap = cent(config.get("onDemandCap"));
    let used = cent(config.get("onDemandUsed"));
    out["prepaidBalanceCents"] = prepaid.map(Value::from).unwrap_or(Value::Null);
    out["onDemandCapCents"] = cap.map(Value::from).unwrap_or(Value::Null);
    out["onDemandUsedCents"] = used.map(Value::from).unwrap_or(Value::Null);

    let mut reset_count: Option<i64> = None;
    for key in ["rateLimitResetCredits", "resetCredits", "availableResets", "resetCreditsAvailable"] {
        let cand_b = billing.get(key);
        let cand_c = config.get(key);
        if let Some(Value::Object(m)) = cand_b.or(cand_c) {
            for fk in ["availableCount", "count", "value"] {
                let n = num(m.get(fk), -1.0) as i64;
                if n >= 0 {
                    reset_count = Some(n);
                    break;
                }
            }
            if reset_count.is_some() {
                break;
            }
        } else if let Some(n) = cand_c.and_then(|v| v.as_i64()).or_else(|| {
            cand_c.and_then(|v| v.as_f64()).map(|f| f as i64)
        }) {
            if n >= 0 {
                reset_count = Some(n);
                break;
            }
        }
    }
    if let Some(n) = reset_count {
        out["resetCreditsAvailable"] = json!(n);
        out["balanceLabel"] = Value::from("Available resets");
        out["balanceValue"] = Value::from(n.to_string());
    } else if let Some(p) = prepaid {
        out["balanceLabel"] = Value::from("Prepaid balance");
        out["balanceValue"] = Value::from(format!("${:.2}", p.abs() as f64 / 100.0));
    } else if cap.unwrap_or(0) > 0 {
        let remaining_cents = (cap.unwrap_or(0) - used.unwrap_or(0)).max(0);
        out["balanceLabel"] = Value::from("On-demand remaining");
        out["balanceValue"] = Value::from(format!("${:.2}", remaining_cents as f64 / 100.0));
    }

    // Per-product breakdown (e.g. GrokBuild / GrokChat) as quota windows so
    // the panel can render them without provider-specific branches.
    if let Some(products) = config.get("productUsage").and_then(|v| v.as_array()) {
        let reset = out["rateLimitResetAt"].as_str().unwrap_or("").to_string();
        let mut windows = Vec::new();
        for entry in products {
            let name = entry
                .get("product")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace("PRODUCT_", "")
                .replace("GROK_", "");
            let pct = num(entry.get("usagePercent"), -1.0);
            if name.is_empty() || pct < 0.0 {
                continue;
            }
            windows.push(json!({
                "label": pretty_product(&name),
                "usedPercent": (pct / 100.0).clamp(0.0, 1.0),
                "resetAt": reset,
            }));
        }
        if !windows.is_empty() {
            out["quotaWindows"] = Value::from(windows);
        }
    }
}

fn pretty_product(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("build") {
        return "Grok Build".to_string();
    }
    if lower.contains("chat") {
        return "Grok Chat".to_string();
    }
    if lower.contains("api") {
        return "API".to_string();
    }
    // Title-case fallback: "VOICE" -> "Voice".
    let mut chars = raw.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_nested_config() {
        let billing = json!({"config": {"creditUsagePercent": 42.5}, "subscriptionTier": "SuperGrok"});
        let cfg = extract_config(&billing).unwrap();
        assert_eq!(cfg["creditUsagePercent"], json!(42.5));
        assert_eq!(tier_of(&billing, &cfg), "SuperGrok");
    }

    #[test]
    fn extracts_bare_config_shape() {
        let billing = json!({"creditUsagePercent": 10.0, "currentPeriod": {"type": "WEEKLY"}});
        assert!(extract_config(&billing).is_some());
    }

    #[test]
    fn rejects_billing_without_config() {
        assert!(extract_config(&json!({"foo": 1})).is_none());
    }

    #[test]
    fn applies_weekly_credits_and_product_windows() {
        let mut out = base();
        let billing = json!({
            "config": {
                "creditUsagePercent": 100.0,
                "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "end": "2026-08-15T01:53:09Z"},
                "onDemandCap": {"val": 0},
                "onDemandUsed": {"val": 0},
                "prepaidBalance": {"val": 0},
                "productUsage": [
                    {"product": "GrokBuild", "usagePercent": 100.0},
                    {"product": "GrokChat"}
                ]
            },
            "subscription_tier": "SuperGrok Heavy"
        });
        let cfg = extract_config(&billing).unwrap();
        apply_billing(&mut out, &billing, &cfg);
        assert_eq!(out["rateLimitPercent"], json!(1.0));
        assert_eq!(out["rateLimitLabel"], json!("Weekly credits"));
        assert_eq!(out["rateLimitResetAt"], json!("2026-08-15T01:53:09Z"));
        assert_eq!(out["tierLabel"], json!("SuperGrok Heavy"));
        let windows = out["quotaWindows"].as_array().unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0]["label"], json!("Grok Build"));
    }

    #[test]
    fn falls_back_to_monthly_limit_ratio() {
        let mut out = base();
        let billing = json!({"config": {"monthlyLimit": {"val": 2000}, "used": {"val": 500}}});
        let cfg = extract_config(&billing).unwrap();
        apply_billing(&mut out, &billing, &cfg);
        assert_eq!(out["rateLimitPercent"], json!(0.25));
        assert_eq!(out["rateLimitLabel"], json!("Monthly credits"));
    }

    #[test]
    fn pretty_products() {
        assert_eq!(pretty_product("GrokBuild"), "Grok Build");
        assert_eq!(pretty_product("PRODUCT_GROK_CHAT"), "Grok Chat");
        assert_eq!(pretty_product("VOICE"), "Voice");
    }
}
