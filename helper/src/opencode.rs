//! OpenCode probe: local SQLite activity (`opencode db path`).

use crate::util::{clean_error, executable_command, remaining, run_command, ProbeError, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

const TOKEN_TOTAL: &str = "COALESCE(json_extract(data, '$.tokens.input'), 0) + COALESCE(json_extract(data, '$.tokens.output'), 0) + COALESCE(json_extract(data, '$.tokens.reasoning'), 0) + COALESCE(json_extract(data, '$.tokens.cache.read'), 0) + COALESCE(json_extract(data, '$.tokens.cache.write'), 0)";
const ROLE: &str = "json_extract(data, '$.role') = 'assistant'";
const MODEL: &str = "CASE WHEN COALESCE(json_extract(data, '$.modelID'), '') = '' THEN 'unknown' WHEN COALESCE(json_extract(data, '$.providerID'), '') = '' THEN json_extract(data, '$.modelID') ELSE json_extract(data, '$.providerID') || '/' || json_extract(data, '$.modelID') END";

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

fn probe_inner(cli_bin: &str, deadline: Instant) -> Result<Value> {
    let mut out = base();
    let cli = executable_command(cli_bin)?;
    let mut db_argv = cli.clone();
    db_argv.extend(["db".to_string(), "path".to_string()]);
    let raw = run_command(&db_argv, remaining(deadline)?)?;
    let home = std::env::var("HOME").unwrap_or_default();
    let db_path = raw
        .lines()
        .rev()
        .map(|l| {
            let t = l.trim();
            if t.starts_with("~/") {
                format!("{home}{}", &t[1..])
            } else {
                t.to_string()
            }
        })
        .find(|p| std::path::Path::new(p).is_file())
        .ok_or_else(|| ProbeError("OpenCode database path was not found".to_string()))?;

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
    if !tables.contains(&"message".to_string()) || !tables.contains(&"session".to_string()) {
        return Err(ProbeError("OpenCode database schema is not supported".to_string()));
    }

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
