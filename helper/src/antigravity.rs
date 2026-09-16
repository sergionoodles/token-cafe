//! Google Antigravity probe: local language-server status / Cloud Code quota.

use crate::util::{clean_error, remaining, run_command, ProbeError, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const LS_SERVICE: &str = "exa.language_server_pb.LanguageServerService";
const CLOUD_CODE_URLS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const FETCH_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const RETRIEVE_QUOTA_PATH: &str = "/v1internal:retrieveUserQuota";
const CC_MODEL_BLACKLIST: [&str; 9] = [
    "MODEL_CHAT_20706",
    "MODEL_CHAT_23310",
    "MODEL_GOOGLE_GEMINI_2_5_FLASH",
    "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING",
    "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE",
    "MODEL_GOOGLE_GEMINI_2_5_PRO",
    "MODEL_PLACEHOLDER_M19",
    "MODEL_PLACEHOLDER_M9",
    "MODEL_PLACEHOLDER_M12",
];

// ---- protobuf-ish varint field decoding (jetski oauth blob) ----

fn read_varint(data: &[u8], mut pos: usize) -> (Option<u64>, usize) {
    let mut value: u64 = 0;
    let mut shift = 0;
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return (Some(value), pos);
        }
        shift += 7;
        if shift > 63 {
            break;
        }
    }
    (None, pos)
}

#[derive(Clone)]
enum Field {
    Varint(u64),
    Bytes(Vec<u8>),
}

fn read_fields(data: &[u8]) -> std::collections::HashMap<u64, Field> {
    let mut fields = std::collections::HashMap::new();
    let mut pos = 0;
    while pos < data.len() {
        let (tag, next) = read_varint(data, pos);
        pos = next;
        let tag = match tag {
            Some(t) => t,
            None => break,
        };
        let num = tag / 8;
        match tag % 8 {
            0 => {
                let (v, next) = read_varint(data, pos);
                pos = next;
                if let Some(v) = v {
                    fields.insert(num, Field::Varint(v));
                } else {
                    break;
                }
            }
            2 => {
                let (len, next) = read_varint(data, pos);
                pos = next;
                let len = match len {
                    Some(l) => l as usize,
                    None => break,
                };
                if pos + len > data.len() {
                    break;
                }
                fields.insert(num, Field::Bytes(data[pos..pos + len].to_vec()));
                pos += len;
            }
            _ => break,
        }
    }
    fields
}

// ---- state db ----

fn sqlite_json_value(db_path: &str, key: &str) -> Option<String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1", [key], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
}

fn resolve_state_db(configured: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut cands = vec![configured.to_string()];
    for c in [
        "~/.config/Antigravity/User/globalStorage/state.vscdb",
        "~/.config/antigravity/User/globalStorage/state.vscdb",
        "~/Library/Application Support/Antigravity/User/globalStorage/state.vscdb",
    ] {
        cands.push(c.to_string());
    }
    for c in cands {
        let expanded = if let Some(rest) = c.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            c
        };
        if !expanded.is_empty() && std::path::Path::new(&expanded).is_file() {
            return expanded;
        }
    }
    if let Some(rest) = configured.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    configured.to_string()
}

struct OauthConfig {
    token_url: String,
    client_id: String,
    client_secret: String,
}

fn load_oauth_config() -> Option<OauthConfig> {
    let env_cfg = OauthConfig {
        token_url: std::env::var("TOKEN_CAFE_ANTIGRAVITY_OAUTH_URL").unwrap_or_default().trim().to_string(),
        client_id: std::env::var("TOKEN_CAFE_ANTIGRAVITY_CLIENT_ID").unwrap_or_default().trim().to_string(),
        client_secret: std::env::var("TOKEN_CAFE_ANTIGRAVITY_CLIENT_SECRET")
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    if !env_cfg.token_url.is_empty() && !env_cfg.client_id.is_empty() && !env_cfg.client_secret.is_empty() {
        return Some(env_cfg);
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut cands = Vec::new();
    let env_path = std::env::var("TOKEN_CAFE_ANTIGRAVITY_OAUTH_CONFIG").unwrap_or_default();
    if !env_path.is_empty() {
        cands.push(env_path);
    }
    if !exe_dir.is_empty() {
        cands.push(format!("{exe_dir}/antigravity_oauth.local.json"));
    }
    cands.push(format!("{home}/.config/token-cafe/antigravity_oauth.json"));
    for cand in cands {
        let raw = match std::fs::read_to_string(&cand) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let cfg = OauthConfig {
            token_url: v.get("token_url").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
            client_id: v.get("client_id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
            client_secret: v
                .get("client_secret")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
        };
        if !cfg.token_url.is_empty() && !cfg.client_id.is_empty() && !cfg.client_secret.is_empty() {
            return Some(cfg);
        }
    }
    None
}

fn load_api_key(db_path: &str) -> Option<String> {
    let raw = sqlite_json_value(db_path, "antigravityAuthStatus")?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("apiKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

struct ProtoTokens {
    access: String,
    refresh: Option<String>,
    expiry: Option<u64>,
}

fn load_proto_tokens(db_path: &str) -> Option<ProtoTokens> {
    let raw = sqlite_json_value(db_path, "jetskiStateSync.agentManagerInitState")?;
    let bytes = base64_decode(&raw).ok()?;
    let outer = read_fields(&bytes);
    let oauth = match outer.get(&6) {
        Some(Field::Bytes(b)) => b.clone(),
        _ => return None,
    };
    let inner = read_fields(&oauth);
    let access = match inner.get(&1) {
        Some(Field::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return None,
    };
    if access.is_empty() {
        return None;
    }
    let refresh = match inner.get(&3) {
        Some(Field::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    };
    let expiry = match inner.get(&4) {
        Some(Field::Bytes(b)) => read_fields(b).get(&1).and_then(|f| match f {
            Field::Varint(v) => Some(*v),
            _ => None,
        }),
        _ => None,
    };
    Some(ProtoTokens { access, refresh, expiry })
}

fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|_| ())
}

// ---- HTTP ----

struct Http {
    client: reqwest::blocking::Client,
    insecure: reqwest::blocking::Client,
}

impl Http {
    fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProbeError(e.to_string()))?;
        let insecure = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| ProbeError(e.to_string()))?;
        Ok(Self { client, insecure })
    }

    /// (status, body). Connection failures -> (0, message).
    fn json(&self, method: &str, url: &str, headers: &[(&str, String)], body: Option<String>, insecure: bool) -> (u16, String) {
        let c = if insecure { &self.insecure } else { &self.client };
        let mut req = match method {
            "POST" => c.post(url),
            _ => c.get(url),
        };
        for (k, v) in headers {
            req = req.header(*k, v.clone());
        }
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").body(b);
        }
        match req.send() {
            Ok(r) => {
                let status = r.status().as_u16();
                (status, r.text().unwrap_or_default())
            }
            Err(e) => (0, e.to_string()),
        }
    }
}

fn request_id() -> String {
    // 16 hex chars from time + pid (only used as an activity tag).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", nanos, std::process::id())
}

// ---- LS discovery ----

fn parse_flag<'a>(command: &'a str, flag: &str) -> Option<String> {
    let needle = format!("{flag} ");
    command.find(&needle).map(|i| {
        command[i + needle.len()..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    })
}

struct Discovery {
    csrf: Option<String>,
    ports: Vec<u16>,
}

fn discover_ls() -> Vec<Discovery> {
    let ps = run_command(
        &["ps".to_string(), "-ax".to_string(), "-o".to_string(), "pid=,command=".to_string()],
        Duration::from_secs(5),
    )
    .unwrap_or_default();
    let mut out = Vec::new();
    for line in ps.lines() {
        let text = line.trim();
        if text.is_empty() || !text.contains("language_server") || !text.to_lowercase().contains("antigravity") {
            continue;
        }
        let mut parts = text.splitn(2, char::is_whitespace);
        let pid: i64 = match parts.next().and_then(|p| p.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let command = parts.next().unwrap_or("").to_string();
        let csrf = parse_flag(&command, "--csrf_token");
        let ext_port: Option<u16> = parse_flag(&command, "--extension_server_port")
            .and_then(|p| p.parse().ok());
        let mut ports: Vec<u16> = discover_ports(pid);
        if let Some(p) = ext_port {
            if !ports.contains(&p) {
                ports.push(p);
            }
        }
        out.push(Discovery { csrf, ports });
    }
    out
}

fn discover_ports(pid: i64) -> Vec<u16> {
    let lsof = run_command(
        &[
            "lsof".to_string(),
            "-nP".to_string(),
            "-iTCP".to_string(),
            "-sTCP:LISTEN".to_string(),
            "-a".to_string(),
            "-p".to_string(),
            pid.to_string(),
        ],
        Duration::from_secs(5),
    )
    .unwrap_or_default();
    let mut ports = Vec::new();
    for line in lsof.lines() {
        // look for ":PORT (LISTEN)"
        if let Some(idx) = line.find("(LISTEN)") {
            let before = &line[..idx];
            if let Some(colon) = before.rfind(':') {
                let num: String = before[colon + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(p) = num.parse::<u16>() {
                    if !ports.contains(&p) {
                        ports.push(p);
                    }
                }
            }
        }
    }
    ports
}

fn probe_port(http: &Http, scheme: &str, port: u16, csrf: Option<&str>) -> bool {
    let (status, _) = http.json(
        "POST",
        &format!("{scheme}://127.0.0.1:{port}/{LS_SERVICE}/GetUnleashData"),
        &[
            ("Content-Type", "application/json".to_string()),
            ("Connect-Protocol-Version", "1".to_string()),
            ("x-codeium-csrf-token", csrf.unwrap_or("").to_string()),
        ],
        Some(json!({"context": {"properties": {"devMode": "false", "extensionVersion": "unknown", "ide": "antigravity", "ideVersion": "unknown", "os": "macos"}}}).to_string()),
        scheme == "https",
    );
    status > 0
}

fn find_working_port(http: &Http, d: &Discovery) -> Option<(u16, &'static str)> {
    for port in &d.ports {
        for scheme in ["https", "http"] {
            if probe_port(http, scheme, *port, d.csrf.as_deref()) {
                return Some((*port, scheme));
            }
        }
    }
    None
}

fn call_ls(http: &Http, port: u16, scheme: &str, csrf: Option<&str>, method: &str, body: Value) -> Option<Value> {
    let (status, text) = http.json(
        "POST",
        &format!("{scheme}://127.0.0.1:{port}/{LS_SERVICE}/{method}"),
        &[
            ("Content-Type", "application/json".to_string()),
            ("Connect-Protocol-Version", "1".to_string()),
            ("x-codeium-csrf-token", csrf.unwrap_or("").to_string()),
        ],
        Some(body.to_string()),
        scheme == "https",
    );
    if !(200..300).contains(&status) {
        return None;
    }
    serde_json::from_str(&text).ok()
}

// ---- quota pools ----

fn normalize_label(label: &str) -> String {
    // Strip a trailing "(...)" suffix.
    let t = label.trim();
    if let Some(i) = t.rfind('(') {
        if t.ends_with(')') {
            return t[..i].trim().to_string();
        }
    }
    t.to_string()
}

fn pool_label(normalized: &str) -> Option<&'static str> {
    let lower = normalized.to_lowercase();
    if lower.contains("gemini") && lower.contains("pro") {
        return Some("Gemini Pro");
    }
    if lower.contains("gemini") && lower.contains("flash") {
        return Some("Gemini Flash");
    }
    None
}

fn model_sort_key(label: &str) -> String {
    let lower = label.to_lowercase();
    if lower.contains("gemini") && lower.contains("pro") {
        return format!("0a_{label}");
    }
    if lower.contains("gemini") {
        return format!("0b_{label}");
    }
    if lower.contains("claude") && lower.contains("opus") {
        return format!("1a_{label}");
    }
    if lower.contains("claude") {
        return format!("1b_{label}");
    }
    format!("2_{label}")
}

#[derive(Clone)]
pub struct Pool {
    pub label: String,
    pub used: f64,
    pub reset: String,
    sort: String,
}

/// Keep only Gemini pools (Claude quota is hidden), dedupe by pool name.
pub fn build_pools(configs: &[Value]) -> Vec<Pool> {
    use std::collections::HashMap;
    let mut deduped: HashMap<String, (f64, String)> = HashMap::new();
    for c in configs {
        let label = c.get("label").and_then(|v| v.as_str()).unwrap_or("").trim();
        if label.is_empty() {
            continue;
        }
        let qi = c.get("quotaInfo").cloned().unwrap_or(Value::Null);
        let remaining = qi.get("remainingFraction").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let pool = match pool_label(&normalize_label(label)) {
            Some(p) => p,
            None => continue,
        };
        let reset = qi.get("resetTime").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let entry = deduped.entry(pool.to_string()).or_insert((remaining.clamp(0.0, 1.0), reset.clone()));
        if remaining < entry.0 {
            *entry = (remaining.clamp(0.0, 1.0), reset);
        }
    }
    let mut pools: Vec<Pool> = deduped
        .into_iter()
        .map(|(label, (rem, reset))| Pool {
            sort: model_sort_key(&label),
            label,
            used: (1.0 - rem).clamp(0.0, 1.0),
            reset,
        })
        .collect();
    pools.sort_by(|a, b| a.sort.cmp(&b.sort));
    pools
}

fn filter_ls_configs(configs: Vec<Value>) -> Vec<Value> {
    configs
        .into_iter()
        .filter(|c| {
            let model_id = c
                .get("modelOrAlias")
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            !CC_MODEL_BLACKLIST.contains(&model_id)
        })
        .collect()
}

fn parse_cloud_code_models(data: &Value) -> Vec<Value> {
    let models = match data.get("models").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut configs = Vec::new();
    for (key, model) in models {
        let model = match model.as_object() {
            Some(m) => m,
            None => continue,
        };
        if model.get("isInternal").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let model_id = model
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(key)
            .to_string();
        if CC_MODEL_BLACKLIST.contains(&model_id.as_str()) {
            continue;
        }
        let display = model.get("displayName").and_then(|v| v.as_str()).unwrap_or("").trim();
        if display.is_empty() {
            continue;
        }
        let qi = model.get("quotaInfo").cloned().unwrap_or(Value::Null);
        configs.push(json!({
            "label": display,
            "quotaInfo": {
                "remainingFraction": qi.get("remainingFraction").cloned().unwrap_or(json!(0)),
                "resetTime": qi.get("resetTime").and_then(|v| v.as_str()).unwrap_or(""),
            },
        }));
    }
    configs
}

fn parse_quota_buckets(data: &Value) -> Vec<Value> {
    let buckets = match data.get("buckets").and_then(|v| v.as_array()) {
        Some(b) => b,
        None => return Vec::new(),
    };
    buckets
        .iter()
        .filter_map(|b| {
            let model_id = b.get("modelId").and_then(|v| v.as_str()).unwrap_or("").trim();
            if model_id.is_empty() {
                return None;
            }
            let label = model_id
                .replace('_', "-")
                .split('-')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            Some(json!({
                "label": label,
                "quotaInfo": {
                    "remainingFraction": b.get("remainingFraction").cloned().unwrap_or(json!(0)),
                    "resetTime": b.get("resetTime").and_then(|v| v.as_str()).unwrap_or(""),
                },
            }))
        })
        .collect()
}

fn refresh_access_token(http: &Http, refresh_token: &str) -> Option<String> {
    let cfg = load_oauth_config()?;
    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        url_encode(&cfg.client_id),
        url_encode(&cfg.client_secret),
        url_encode(refresh_token),
    );
    let (status, text) = http.json("POST", &cfg.token_url, &[("Content-Type", "application/x-www-form-urlencoded".to_string())], Some(body), false);
    if !(200..300).contains(&status) {
        return None;
    }
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn probe_cloud_code_with(http: &Http, token: &str, project_id: &str, base_urls: &[&str]) -> Option<Value> {
    let mut saw_auth = false;
    for base in base_urls {
        let mut reqs: Vec<(&str, Value, &str)> = Vec::new();
        if !project_id.is_empty() {
            reqs.push((RETRIEVE_QUOTA_PATH, json!({"project": project_id}), "quota"));
        }
        reqs.push((
            FETCH_MODELS_PATH,
            if project_id.is_empty() { json!({}) } else { json!({"project": project_id}) },
            "models",
        ));
        for (path, body, rtype) in reqs {
            let (status, text) = http.json(
                "POST",
                &format!("{base}{path}"),
                &[
                    ("Authorization", format!("Bearer {token}")),
                    ("User-Agent", "antigravity/cli/1.0.3".to_string()),
                    ("x-activity-request-id", request_id()),
                ],
                Some(body.to_string()),
                false,
            );
            if status == 401 || status == 403 {
                saw_auth = true;
                continue;
            }
            if (200..300).contains(&status) {
                let mut data: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if rtype == "quota" && data.get("buckets").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(true) {
                    continue;
                }
                if rtype == "models" && data.get("models").and_then(|v| v.as_object()).map(|o| o.is_empty()).unwrap_or(true) {
                    continue;
                }
                data["_tokenCafeResponseType"] = Value::from(rtype);
                return Some(data);
            }
        }
    }
    if saw_auth {
        Some(json!({"authFailed": true}))
    } else {
        None
    }
}

fn probe_cloud_code(http: &Http, token: &str, project_id: &str) -> Option<Value> {
    probe_cloud_code_with(http, token, project_id, &CLOUD_CODE_URLS)
}

fn probe_ls(http: &Http, api_key: Option<&str>) -> Option<(String, Vec<Pool>)> {
    for d in discover_ls() {
        let (port, scheme) = find_working_port(http, &d)?;
        let mut metadata = json!({
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en",
        });
        if let Some(k) = api_key {
            metadata["apiKey"] = Value::from(k);
        }
        let data = call_ls(http, port, scheme, d.csrf.as_deref(), "GetUserStatus", json!({"metadata": metadata}));
        let has_user = data.as_ref().and_then(|v| v.get("userStatus")).is_some();
        let (configs, plan) = if has_user {
            let us = data.as_ref().and_then(|v| v.get("userStatus")).cloned().unwrap_or(Value::Null);
            let configs = us
                .get("cascadeModelConfigData")
                .and_then(|v| v.get("clientModelConfigs"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let plan = us
                .get("planStatus")
                .and_then(|v| v.get("planInfo"))
                .and_then(|v| v.get("planName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (configs, plan)
        } else {
            let data = call_ls(
                http,
                port,
                scheme,
                d.csrf.as_deref(),
                "GetCommandModelConfigs",
                json!({"metadata": metadata}),
            );
            let configs = data
                .as_ref()
                .and_then(|v| v.get("clientModelConfigs"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (configs, String::new())
        };
        let pools = build_pools(&filter_ls_configs(configs));
        if !pools.is_empty() {
            return Some((plan, pools));
        }
    }
    None
}

fn base_result() -> Value {
    json!({
        "ready": false,
        "usageStatusText": "",
        "rateLimitPercent": -1,
        "rateLimitLabel": "Quota",
        "rateLimitResetAt": "",
        "secondaryRateLimitPercent": -1,
        "secondaryRateLimitLabel": "",
        "secondaryRateLimitResetAt": "",
        "quotaWindows": [],
        "todayPrompts": 0,
        "todaySessions": 0,
        "todayTotalTokens": 0,
        "todayTokensByModel": {},
        "recentDays": [],
        "totalPrompts": 0,
        "totalSessions": 0,
        "modelUsage": {},
        "tierLabel": "",
    })
}

fn apply_pools(mut out: Value, pools: &[Pool], tier: &str) -> Value {
    if pools.is_empty() {
        return out;
    }
    out["quotaWindows"] = Value::from(
        pools
            .iter()
            .map(|p| json!({"label": p.label, "usedPercent": p.used, "resetAt": p.reset}))
            .collect::<Vec<_>>(),
    );
    out["rateLimitPercent"] = json!(pools[0].used);
    out["rateLimitLabel"] = Value::from(pools[0].label.clone());
    out["rateLimitResetAt"] = Value::from(pools[0].reset.clone());
    if pools.len() > 1 {
        out["secondaryRateLimitPercent"] = json!(pools[1].used);
        out["secondaryRateLimitLabel"] = Value::from(pools[1].label.clone());
        out["secondaryRateLimitResetAt"] = Value::from(pools[1].reset.clone());
    }
    out["tierLabel"] = Value::from(tier);
    out["ready"] = Value::from(true);
    out
}

pub fn probe(state_db: &str, project_id: &str, deadline: Instant) -> Value {
    let mut v = match probe_inner(state_db, project_id, deadline) {
        Ok(v) => v,
        Err(e) => {
            let mut v = base_result();
            v["usageStatusText"] = Value::from(clean_error(e.0));
            v
        }
    };
    // Recency for "most recently used" bar ordering: state DB mtime tracks
    // last IDE activity. Only fill when the probe did not report better.
    if v.get("lastUsedAt").and_then(|n| n.as_i64()).unwrap_or(0) <= 0 {
        let resolved = resolve_state_db(state_db);
        let m = crate::util::file_mtime_secs(std::path::Path::new(&resolved));
        if m > 0 {
            v["lastUsedAt"] = serde_json::json!(m);
        }
    }
    v
}

fn probe_inner(state_db: &str, project_id: &str, deadline: Instant) -> Result<Value> {
    let mut out = base_result();
    let http = Http::new(remaining(deadline)?.min(Duration::from_secs(15)))?;
    let state_db = resolve_state_db(state_db);
    let mut project_id = project_id.trim().to_string();
    if project_id.is_empty() {
        for key in ["OPENCODE_AGY_PROJECT_ID", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"] {
            project_id = std::env::var(key).unwrap_or_default().trim().to_string();
            if !project_id.is_empty() {
                break;
            }
        }
    }
    let api_key = load_api_key(&state_db);
    let proto = load_proto_tokens(&state_db);
    let refresh_token = proto.as_ref().and_then(|p| p.refresh.clone());
    let has_refresh = refresh_token.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
    let has_oauth = load_oauth_config().is_some();

    if let Some((tier, pools)) = probe_ls(&http, api_key.as_deref()) {
        return Ok(apply_pools(out, &pools, &tier));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut tokens: Vec<String> = Vec::new();
    if let Some(p) = &proto {
        let fresh = p.expiry.map(|e| e as u64 > now).unwrap_or(true);
        if fresh {
            tokens.push(p.access.clone());
        }
    }
    if let Some(k) = &api_key {
        if !tokens.contains(k) {
            tokens.push(k.clone());
        }
    }
    let mut cloud: Option<Value> = None;
    for t in &tokens {
        if let Some(d) = probe_cloud_code(&http, t, &project_id) {
            if d.get("authFailed").is_none() {
                cloud = Some(d);
                break;
            }
        }
    }
    if cloud.is_none() {
        if let (true, true, Some(rt)) = (has_refresh, has_oauth, refresh_token) {
            if let Some(new_token) = refresh_access_token(&http, &rt) {
                if let Some(d) = probe_cloud_code(&http, &new_token, &project_id) {
                    if d.get("authFailed").is_none() {
                        cloud = Some(d);
                    }
                }
            }
        }
    }
    if let Some(data) = cloud {
        let rtype = data.get("_tokenCafeResponseType").and_then(|v| v.as_str()).unwrap_or("");
        let configs = if rtype == "quota" {
            parse_quota_buckets(&data)
        } else {
            parse_cloud_code_models(&data)
        };
        return Ok(apply_pools(out, &build_pools(&configs), ""));
    }
    if has_refresh && !has_oauth {
        out["usageStatusText"] = Value::from("Add Antigravity OAuth config to enable refresh-token fallback");
        return Ok(out);
    }
    out["usageStatusText"] = Value::from("Start Antigravity and try again");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_claude_quota_pool() {
        let pools = build_pools(&[
            json!({"label": "Claude Opus", "quotaInfo": {"remainingFraction": 0.5}}),
            json!({"label": "Gemini Pro", "quotaInfo": {"remainingFraction": 0.5}}),
        ]);
        assert_eq!(pools.iter().map(|p| p.label.clone()).collect::<Vec<_>>(), vec!["Gemini Pro"]);
    }

    #[test]
    fn quota_auth_error_falls_back_to_models() {
        let (server, base) = mock_server(vec![
            (403, "denied".to_string()),
            (
                200,
                json!({"models": {"gemini": {"displayName": "Gemini Pro"}}}).to_string(),
            ),
        ]);
        let http = Http::new(Duration::from_secs(5)).unwrap();
        let data = probe_cloud_code_with(&http, "token", "project-id", &[&base]).unwrap();
        assert_eq!(data["_tokenCafeResponseType"], json!("models"));
        drop(server);
    }

    #[test]
    fn empty_quota_falls_back_to_models() {
        let (server, base) = mock_server(vec![
            (200, "{}".to_string()),
            (200, json!({"models": {"gemini": {}}}).to_string()),
        ]);
        let http = Http::new(Duration::from_secs(5)).unwrap();
        let data = probe_cloud_code_with(&http, "token", "project-id", &[&base]).unwrap();
        assert_eq!(data["_tokenCafeResponseType"], json!("models"));
        drop(server);
    }

    /// Minimal canned-response HTTP server over std TcpListener.
    fn mock_server(responses: Vec<(u16, String)>) -> (std::thread::JoinHandle<()>, String) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            for stream in listener.incoming().take(4) {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let content_len = req
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                // Drain body bytes already buffered + remainder if needed.
                let header_end = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                let mut remaining_body = content_len.saturating_sub(buf[..n].len().saturating_sub(header_end));
                while remaining_body > 0 {
                    let mut tmp = [0u8; 1024];
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => remaining_body -= remaining_body.min(k),
                    }
                }
                let (status, body) = responses.next().unwrap_or((404, String::new()));
                let reason = if status == 200 { "OK" } else { "Forbidden" };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (handle, base)
    }
}
