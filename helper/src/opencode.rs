//! OpenCode probe: local SQLite activity.
//!
//! v2 removed `opencode db path`, and moved live data from the legacy
//! `message`/`session` tables to `session_message`/`session_v2`. The DB
//! path is resolved directly (XDG data dir, then the default location,
//! then an `opencode debug paths` hint), and queries target
//! `session_message` when present with a fallback to the v1 tables.

use crate::util::{
    clean_error, executable_command, expand_home, remaining, run_command, ProbeError, Result,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

const TOKEN_TOTAL: &str = "COALESCE(json_extract(data, '$.tokens.input'), 0) + COALESCE(json_extract(data, '$.tokens.output'), 0) + COALESCE(json_extract(data, '$.tokens.reasoning'), 0) + COALESCE(json_extract(data, '$.tokens.cache.read'), 0) + COALESCE(json_extract(data, '$.tokens.cache.write'), 0)";
const ROLE: &str = "json_extract(data, '$.role') = 'assistant'";
const MODEL: &str = "CASE WHEN COALESCE(json_extract(data, '$.modelID'), '') = '' THEN 'unknown' WHEN COALESCE(json_extract(data, '$.providerID'), '') = '' THEN json_extract(data, '$.modelID') ELSE json_extract(data, '$.providerID') || '/' || json_extract(data, '$.modelID') END";
// v2 (OpenCode 2.x): assistant turns live in `session_message`, selected
// by the `type` column; the model ref moved under `$.model`.
const V2_ROLE: &str = "type = 'assistant'";
const V2_MODEL: &str = "CASE WHEN COALESCE(json_extract(data, '$.model.id'), '') = '' THEN 'unknown' WHEN COALESCE(json_extract(data, '$.model.providerID'), '') = '' THEN json_extract(data, '$.model.id') ELSE json_extract(data, '$.model.providerID') || '/' || json_extract(data, '$.model.id') END";

fn i64_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(n.as_f64().unwrap_or(0.0) as i64),
        _ => 0,
    }
}

fn f64_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Local-midnight window [start_ms, end_ms) as epoch millis, via libc.
fn today_window_ms() -> (i64, i64) {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        let start = libc::mktime(&mut tm) as i64;
        (start * 1000, (start + 86400) * 1000)
    }
}

fn local_day_string(offset_days: i64) -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut()) + offset_days * 86400;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
    }
}

fn one_row(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<BTreeMap<String, Value>> {
    let mut stmt = conn.prepare(sql).map_err(|e| ProbeError(e.to_string()))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params).map_err(|e| ProbeError(e.to_string()))?;
    let mut map = BTreeMap::new();
    if let Some(row) = rows.next().map_err(|e| ProbeError(e.to_string()))? {
        for (i, name) in cols.iter().enumerate() {
            let raw: rusqlite::types::Value = row.get(i).map_err(|e| ProbeError(e.to_string()))?;
            let v = match raw {
                rusqlite::types::Value::Null => Value::Null,
                rusqlite::types::Value::Integer(n) => json!(n),
                rusqlite::types::Value::Real(f) => json!(f),
                rusqlite::types::Value::Text(s) => Value::from(s),
                rusqlite::types::Value::Blob(_) => Value::Null,
            };
            map.insert(name.clone(), v);
        }
    }
    Ok(map)
}

pub fn probe(cli_bin: &str, deadline: Instant) -> Value {
    match probe_inner(cli_bin, deadline) {
        Ok(v) => v,
        Err(e) => {
            let mut v = base();
            v["usageStatusText"] = Value::from(clean_error(e.0));
            // Error path: fall back to the DB file mtime so a broken query
            // still yields a recency signal for "most recently used".
            for cand in candidate_db_paths() {
                let m = crate::util::file_mtime_secs(&std::path::PathBuf::from(cand));
                if m > 0 {
                    v["lastUsedAt"] = serde_json::json!(m);
                    break;
                }
            }
            v
        }
    }
}

fn base() -> Value {
    json!({
        "provider": "opencode",
        "ready": false,
        "usageStatusText": "",
        "rateLimitPercent": -1,
        "rateLimitLabel": "Local usage",
        "rateLimitResetAt": "",
        "todayPrompts": 0,
        "todaySessions": 0,
        "todayTotalTokens": 0,
        "todayTokensByModel": {},
        "recentDays": [],
        "totalPrompts": 0,
        "totalMessages": 0,
        "totalSessions": 0,
        "modelUsage": {},
        "todayCost": 0,
        "totalCost": 0,
        "tierLabel": "",
        "balanceLabel": "Today's cost",
        "balanceValue": "$0.00",
    })
}

/// Candidate DB locations, in priority order. `opencode debug paths`
/// reports the same default; XDG first so custom data dirs work.
fn candidate_db_paths() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        let xdg = xdg.trim();
        if !xdg.is_empty() {
            out.push(format!("{xdg}/opencode/opencode.db"));
        }
    }
    out.push(expand_home("~/.local/share/opencode/opencode.db"));
    out
}

/// Parse the `db` line out of `opencode debug paths` output
/// (lines look like `db         /home/u/.local/share/opencode/opencode.db`).
fn parse_debug_paths_db(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let mut cols = line.split_whitespace();
        if cols.next() == Some("db") {
            if let Some(path) = cols.next() {
                let home = std::env::var("HOME").unwrap_or_default();
                if path.starts_with("~/") {
                    return Some(format!("{home}{}", &path[1..]));
                }
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Best-effort DB hint from the CLI for non-default data dirs.
/// Never fails: returns None when the CLI is missing or errors.
fn debug_paths_db(cli_bin: &str, deadline: Instant) -> Option<String> {
    let cli = executable_command(cli_bin).ok()?;
    let budget = remaining(deadline).ok()?;
    let budget = budget.min(std::time::Duration::from_secs(5));
    let mut argv = cli;
    argv.extend(["debug".to_string(), "paths".to_string()]);
    let raw = run_command(&argv, budget).ok()?;
    let path = parse_debug_paths_db(&raw)?;
    if std::path::Path::new(&path).is_file() {
        Some(path)
    } else {
        None
    }
}

fn resolve_db_path(cli_bin: &str, deadline: Instant) -> Result<String> {
    for cand in candidate_db_paths() {
        if std::path::Path::new(&cand).is_file() {
            return Ok(cand);
        }
    }
    if let Some(hint) = debug_paths_db(cli_bin, deadline) {
        return Ok(hint);
    }
    Err(ProbeError(
        "OpenCode database path was not found".to_string(),
    ))
}

fn probe_inner(cli_bin: &str, deadline: Instant) -> Result<Value> {
    let out = base();
    let db_path = resolve_db_path(cli_bin, deadline)?;

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ProbeError(e.to_string()))?;
    conn.execute_batch("PRAGMA query_only = ON;")
        .map_err(|e| ProbeError(e.to_string()))?;
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|e| ProbeError(e.to_string()))?
        .query_map([], |r| r.get(0))
        .map_err(|e| ProbeError(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    if tables.contains(&"session_message".to_string()) {
        return probe_v2(&conn, out);
    }
    if !tables.contains(&"message".to_string()) || !tables.contains(&"session".to_string()) {
        return Err(ProbeError("OpenCode database schema is not supported".to_string()));
    }
    probe_v1(&conn, out)
}

/// Legacy v1 tables (`message`/`session`, assistant selected by `$.role`).
fn probe_v1(conn: &Connection, mut out: Value) -> Result<Value> {
    let (start_ms, end_ms) = today_window_ms();
    let row = one_row(
        &conn,
        &format!(
            "SELECT COUNT(*) requests, COUNT(DISTINCT session_id) sessions, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens, COALESCE(SUM(COALESCE(json_extract(data, '$.cost'), 0)), 0) cost FROM message WHERE {ROLE} AND time_created >= ?1 AND time_created < ?2"
        ),
        &[&start_ms, &end_ms],
    )?;
    out["todayPrompts"] = json!(i64_of(row.get("requests")));
    out["todaySessions"] = json!(i64_of(row.get("sessions")));
    out["todayTotalTokens"] = json!(i64_of(row.get("tokens")));
    let today_cost = f64_of(row.get("cost"));
    out["todayCost"] = json!(today_cost);
    out["balanceValue"] = Value::from(format!("${today_cost:.2}"));

    let totals = one_row(
        &conn,
        &format!(
            "SELECT COUNT(*) requests, COALESCE(SUM(COALESCE(json_extract(data, '$.cost'), 0)), 0) cost FROM message WHERE {ROLE}"
        ),
        &[],
    )?;
    out["totalPrompts"] = json!(i64_of(totals.get("requests")));
    out["totalCost"] = json!(f64_of(totals.get("cost")));
    let n_msg: i64 = conn
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .map_err(|e| ProbeError(e.to_string()))?;
    let n_sess: i64 = conn
        .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
        .map_err(|e| ProbeError(e.to_string()))?;
    out["totalMessages"] = json!(n_msg);
    out["totalSessions"] = json!(n_sess);
    // Recency for "most recently used" bar ordering (epoch seconds).
    let last_ms: i64 = conn
        .query_row(
            &format!("SELECT COALESCE(MAX(time_created), 0) FROM message WHERE {ROLE}"),
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if last_ms > 0 {
        out["lastUsedAt"] = json!(last_ms / 1000);
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {MODEL} model, SUM(COALESCE(json_extract(data, '$.tokens.input'), 0)) it, SUM(COALESCE(json_extract(data, '$.tokens.output'), 0) + COALESCE(json_extract(data, '$.tokens.reasoning'), 0)) ot, SUM(COALESCE(json_extract(data, '$.tokens.cache.read'), 0)) cr, SUM(COALESCE(json_extract(data, '$.tokens.cache.write'), 0)) cw FROM message WHERE {ROLE} GROUP BY model ORDER BY model"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut model_usage = serde_json::Map::new();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (model, it, ot, cr, cw) = r.map_err(|e| ProbeError(e.to_string()))?;
        model_usage.insert(
            model,
            json!({"inputTokens": it, "outputTokens": ot, "cacheReadInputTokens": cr, "cacheCreationInputTokens": cw}),
        );
    }
    out["modelUsage"] = Value::from(model_usage);

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {MODEL} model, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens FROM message WHERE {ROLE} AND time_created >= ?1 AND time_created < ?2 GROUP BY model ORDER BY model"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut today_models = serde_json::Map::new();
    let rows = stmt
        .query_map([start_ms, end_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))))
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (model, toks) = r.map_err(|e| ProbeError(e.to_string()))?;
        today_models.insert(model, json!(toks));
    }
    out["todayTokensByModel"] = Value::from(today_models);

    let six_ago = start_ms - 6 * 86400 * 1000;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT date(time_created / 1000, 'unixepoch', 'localtime') date, COUNT(*) requests, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens FROM message WHERE {ROLE} AND time_created >= ?1 GROUP BY date"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut daily: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let rows = stmt
        .query_map([six_ago], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (date, reqs, toks) = r.map_err(|e| ProbeError(e.to_string()))?;
        daily.insert(date, (reqs, toks));
    }
    let mut recent = Vec::new();
    for offset in 0..7 {
        let date = local_day_string(offset - 6);
        let (reqs, toks) = daily.get(&date).copied().unwrap_or((0, 0));
        recent.push(json!({"date": date, "messageCount": reqs, "tokens": toks}));
    }
    out["recentDays"] = Value::from(recent);

    out["ready"] = Value::from(true);
    out["usageStatusText"] = Value::from("");
    Ok(out)
}

/// v2 tables (`session_message`/`session_v2`, assistant selected by the
/// `type` column). Same output shape as v1 so widget/panel need no changes.
fn probe_v2(conn: &Connection, mut out: Value) -> Result<Value> {
    let (start_ms, end_ms) = today_window_ms();
    let row = one_row(
        conn,
        &format!(
            "SELECT COUNT(*) requests, COUNT(DISTINCT session_id) sessions, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens, COALESCE(SUM(COALESCE(json_extract(data, '$.cost'), 0)), 0) cost FROM session_message WHERE {V2_ROLE} AND time_created >= ?1 AND time_created < ?2"
        ),
        &[&start_ms, &end_ms],
    )?;
    out["todayPrompts"] = json!(i64_of(row.get("requests")));
    out["todaySessions"] = json!(i64_of(row.get("sessions")));
    out["todayTotalTokens"] = json!(i64_of(row.get("tokens")));
    let today_cost = f64_of(row.get("cost"));
    out["todayCost"] = json!(today_cost);
    out["balanceValue"] = Value::from(format!("${today_cost:.2}"));

    let totals = one_row(
        conn,
        &format!(
            "SELECT COUNT(*) requests, COALESCE(SUM(COALESCE(json_extract(data, '$.cost'), 0)), 0) cost FROM session_message WHERE {V2_ROLE}"
        ),
        &[],
    )?;
    out["totalPrompts"] = json!(i64_of(totals.get("requests")));
    out["totalCost"] = json!(f64_of(totals.get("cost")));
    let n_msg: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_message", [], |r| r.get(0))
        .map_err(|e| ProbeError(e.to_string()))?;
    // session_v2 may be mid-migration; fall back to distinct sessions.
    let n_sess: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_v2", [], |r| r.get(0))
        .or_else(|_| {
            conn.query_row(
                "SELECT COUNT(DISTINCT session_id) FROM session_message",
                [],
                |r| r.get(0),
            )
        })
        .map_err(|e| ProbeError(e.to_string()))?;
    out["totalMessages"] = json!(n_msg);
    out["totalSessions"] = json!(n_sess);
    // Recency for "most recently used" bar ordering (epoch seconds).
    // Any row type counts: a pending user prompt is activity too.
    let last_ms: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(time_created), 0) FROM session_message",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if last_ms > 0 {
        out["lastUsedAt"] = json!(last_ms / 1000);
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {V2_MODEL} model, SUM(COALESCE(json_extract(data, '$.tokens.input'), 0)) it, SUM(COALESCE(json_extract(data, '$.tokens.output'), 0) + COALESCE(json_extract(data, '$.tokens.reasoning'), 0)) ot, SUM(COALESCE(json_extract(data, '$.tokens.cache.read'), 0)) cr, SUM(COALESCE(json_extract(data, '$.tokens.cache.write'), 0)) cw FROM session_message WHERE {V2_ROLE} GROUP BY model ORDER BY model"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut model_usage = serde_json::Map::new();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (model, it, ot, cr, cw) = r.map_err(|e| ProbeError(e.to_string()))?;
        model_usage.insert(
            model,
            json!({"inputTokens": it, "outputTokens": ot, "cacheReadInputTokens": cr, "cacheCreationInputTokens": cw}),
        );
    }
    out["modelUsage"] = Value::from(model_usage);

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {V2_MODEL} model, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens FROM session_message WHERE {V2_ROLE} AND time_created >= ?1 AND time_created < ?2 GROUP BY model ORDER BY model"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut today_models = serde_json::Map::new();
    let rows = stmt
        .query_map([start_ms, end_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))))
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (model, toks) = r.map_err(|e| ProbeError(e.to_string()))?;
        today_models.insert(model, json!(toks));
    }
    out["todayTokensByModel"] = Value::from(today_models);

    let six_ago = start_ms - 6 * 86400 * 1000;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT date(time_created / 1000, 'unixepoch', 'localtime') date, COUNT(*) requests, COALESCE(SUM({TOKEN_TOTAL}), 0) tokens FROM session_message WHERE {V2_ROLE} AND time_created >= ?1 GROUP BY date"
        ))
        .map_err(|e| ProbeError(e.to_string()))?;
    let mut daily: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let rows = stmt
        .query_map([six_ago], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })
        .map_err(|e| ProbeError(e.to_string()))?;
    for r in rows {
        let (date, reqs, toks) = r.map_err(|e| ProbeError(e.to_string()))?;
        daily.insert(date, (reqs, toks));
    }
    let mut recent = Vec::new();
    for offset in 0..7 {
        let date = local_day_string(offset - 6);
        let (reqs, toks) = daily.get(&date).copied().unwrap_or((0, 0));
        recent.push(json!({"date": date, "messageCount": reqs, "tokens": toks}));
    }
    out["recentDays"] = Value::from(recent);

    out["ready"] = Value::from(true);
    out["usageStatusText"] = Value::from("");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_db_line_from_debug_paths() {
        let raw = "bin        /home/u/.cache/opencode/bin\n\
                   cache      /home/u/.cache/opencode\n\
                   data       /home/u/.local/share/opencode\n\
                   db         /home/u/.local/share/opencode/opencode.db\n\
                   log        /home/u/.local/share/opencode/log\n";
        assert_eq!(
            parse_debug_paths_db(raw).as_deref(),
            Some("/home/u/.local/share/opencode/opencode.db")
        );
        assert_eq!(parse_debug_paths_db("nothing here\n"), None);
        assert_eq!(parse_debug_paths_db(""), None);
    }

    #[test]
    fn candidates_end_with_default_location() {
        let cands = candidate_db_paths();
        assert!(!cands.is_empty());
        assert_eq!(
            cands.last().map(|s| s.as_str()),
            Some(expand_home("~/.local/share/opencode/opencode.db").as_str())
        );
    }
}
