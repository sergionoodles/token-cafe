//! Shared helpers: errors, deadlines, login-shell PATH bootstrap,
//! CLI resolution, bounded subprocesses, timestamps, JSON numbers.

use serde_json::Value;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ProbeError(pub String);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<serde_json::Error> for ProbeError {
    fn from(e: serde_json::Error) -> Self {
        ProbeError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ProbeError>;

/// Collapse whitespace and cap length, mirroring the old Python `clean_error`.
pub fn clean_error(msg: impl AsRef<str>) -> String {
    let collapsed = msg.as_ref().split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(300).collect();
    if out.is_empty() {
        out = "Probe failed".to_string();
    }
    out
}

pub fn error_result(provider: &str, err: impl std::fmt::Display) -> Value {
    serde_json::json!({
        "provider": provider,
        "ready": false,
        "usageStatusText": clean_error(err.to_string()),
        "rateLimitPercent": -1,
    })
}

/// Remaining time until `deadline`, or a timeout error.
pub fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| ProbeError("Probe timed out".to_string()))
}

/// Best-effort login-shell PATH (version-manager shims etc.).
/// Noctalia can start with a reduced desktop-session environment.
pub fn user_shell_path(timeout: Duration) -> String {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.is_empty() {
        return String::new();
    }
    let mut child = match Command::new(&shell)
        .args(["-lic", "printf '\\0'; env -0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let mut out = Vec::new();
    let status = match child
        .stdout
        .take()
        .map(|mut s| s.read_to_end(&mut out).ok())
    {
        _ => wait_timeout(&mut child, timeout),
    };
    match status {
        Some(true) => parse_path_from_env0(&out).unwrap_or_default(),
        _ => {
            kill_group(child.id());
            String::new()
        }
    }
}

pub fn parse_path_from_env0(out: &[u8]) -> Option<String> {
    // First NUL-terminated record is the shell banner; entries follow.
    let mut parts = out.split(|b| *b == 0);
    parts.next()?;
    for entry in parts {
        if let Some(rest) = entry.strip_prefix(b"PATH=") {
            return Some(String::from_utf8_lossy(rest).into_owned());
        }
    }
    None
}

pub fn configure_user_path(timeout: Duration) {
    let path = user_shell_path(timeout);
    if !path.is_empty() {
        std::env::set_var("PATH", path);
    }
}

/// Polling wait with a timeout. Returns Some(exit_ok).
fn wait_timeout(child: &mut std::process::Child, timeout: Duration) -> Option<bool> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Some(false),
        }
    }
}

fn kill_group(pid: u32) {
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
    }
}

/// Minimal shell-word splitter (whitespace + single/double quotes).
pub fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            // Executable bit check (owner/group/other).
            if let Ok(md) = std::fs::metadata(&cand) {
                use std::os::unix::fs::PermissionsExt;
                if md.permissions().mode() & 0o111 != 0 {
                    return Some(cand.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// Resolve a configured CLI value to argv, expanding `~` and PATH.
pub fn executable_command(value: &str) -> Result<Vec<String>> {
    let mut parts = split_words(value);
    if parts.is_empty() {
        return Err(ProbeError("CLI binary is empty".to_string()));
    }
    let mut first = parts[0].clone();
    if first.starts_with('~') {
        if let Some(home) = std::env::var_os("HOME") {
            first = format!("{}{}", home.to_string_lossy(), &first[1..]);
        }
    }
    if first.contains('/') {
        if !std::path::Path::new(&first).is_file() {
            return Err(ProbeError(format!("CLI not found: {first}")));
        }
    } else {
        first = which(&first).ok_or_else(|| ProbeError(format!("CLI not found: {first}")))?;
    }
    parts[0] = first;
    Ok(parts)
}

/// Run a command to completion with a timeout, returning trimmed stdout.
pub fn run_command(argv: &[String], timeout: Duration) -> Result<String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| ProbeError(format!("Could not start CLI: {e}")))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child.stdout.take().map(|mut s| s.read_to_end(&mut stdout));
    child.stderr.take().map(|mut s| s.read_to_end(&mut stderr));
    match wait_timeout(&mut child, timeout) {
        Some(true) => Ok(String::from_utf8_lossy(&stdout).trim().to_string()),
        Some(false) => {
            let detail = String::from_utf8_lossy(&stderr).trim().to_string();
            let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            Err(ProbeError(if detail.is_empty() {
                format!("CLI exited with status {code}")
            } else {
                detail
            }))
        }
        None => {
            kill_group(child.id());
            let _ = child.wait();
            Err(ProbeError(format!(
                "Command timed out after {}s",
                timeout.as_secs_f64()
            )))
        }
    }
}

/// HOME directory, empty when unset.
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Expand a leading `~/` using HOME.
pub fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = home_dir();
        if home.is_empty() {
            return path.to_string();
        }
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// File mtime as epoch seconds, 0 on any error.
pub fn file_mtime_secs(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Max mtime (epoch secs) under `dir`, recursive, bounded by `max_files`.
/// Returns 0 when the dir is missing/unreadable. Follows no symlinks
/// beyond one level (symlinks to files count via metadata, dirs are
/// not descended into when symlinked to avoid cycles).
pub fn dir_max_mtime(dir: &str, max_files: usize) -> i64 {
    let root = expand_home(dir);
    let mut best: i64 = 0;
    let mut stack = vec![std::path::PathBuf::from(&root)];
    let mut seen: usize = 0;
    while let Some(path) = stack.pop() {
        let entries = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if seen >= max_files {
                return best;
            }
            seen += 1;
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                // Do not descend into symlinked dirs (cycle safety).
                if p.is_symlink() {
                    continue;
                }
                stack.push(p);
            } else if ft.is_file() || ft.is_symlink() {
                let m = file_mtime_secs(&p);
                if m > best {
                    best = m;
                }
            }
        }
    }
    best
}

/// f64-or-default, mirroring the old `number()` helper (NaN -> default).
pub fn num(v: Option<&Value>, default: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(default),
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        _ => default,
    }
}

/// Days since civil 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// ISO-8601 (or epoch millis/seconds) to `…Z` UTC string, else "".
pub fn iso_timestamp(v: Option<&Value>) -> String {
    match v {
        None => return String::new(),
        Some(Value::Null) => return String::new(),
        Some(Value::Number(n)) => {
            let mut ms = n.as_f64().unwrap_or(f64::NAN);
            if !ms.is_finite() {
                return String::new();
            }
            if ms < 1e12 {
                ms *= 1000.0;
            }
            return format_epoch_ms(ms as i64);
        }
        Some(Value::String(s)) if s.trim().is_empty() => return String::new(),
        Some(Value::String(s)) => {
            if let Ok(ms) = parse_iso_to_ms(s) {
                return format_epoch_ms(ms);
            }
            // Maybe a numeric string.
            if let Ok(n) = s.trim().parse::<f64>() {
                return iso_timestamp(Some(&Value::from(n)));
            }
            return String::new();
        }
        _ => return String::new(),
    }
}

fn parse_iso_to_ms(s: &str) -> std::result::Result<i64, ()> {
    let s = s.trim();
    // Split trailing zone: Z or ±HH:MM / ±HHMM.
    let (body, off_min): (&str, i64) = if s.ends_with('Z') || s.ends_with('z') {
        (&s[..s.len() - 1], 0)
    } else if s.len() > 5 {
        let tail5 = &s[s.len() - 5..];
        let tail6 = if s.len() > 6 { &s[s.len() - 6..] } else { "" };
        let mut found: Option<(&str, i64)> = None;
        for tail in [tail6, tail5] {
            if tail.len() >= 5 {
                let (sign, rest) = match tail.as_bytes()[0] {
                    b'+' => (1i64, &tail[1..]),
                    b'-' => (-1i64, &tail[1..]),
                    _ => continue,
                };
                let digits: String = rest.chars().filter(|c| *c != ':').collect();
                if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
                    let hh: i64 = digits[..2].parse().map_err(|_| ())?;
                    let mm: i64 = digits[2..].parse().map_err(|_| ())?;
                    found = Some((&s[..s.len() - tail.len()], sign * (hh * 60 + mm)));
                    break;
                }
            }
        }
        found.ok_or(())?
    } else {
        (s, 0)
    };
    // body: YYYY-MM-DD[T ]HH:MM[:SS[.frac]]
    let (date, time) = body
        .find(['T', ' '])
        .map(|i| (&body[..i], &body[i + 1..]))
        .ok_or(())?;
    let mut di = date.split('-');
    let y: i64 = di.next().ok_or(())?.parse().map_err(|_| ())?;
    let mo: i64 = di.next().ok_or(())?.parse().map_err(|_| ())?;
    let d: i64 = di.next().ok_or(())?.parse().map_err(|_| ())?;
    let mut ti = time.split(':');
    let h: i64 = ti.next().ok_or(())?.parse().map_err(|_| ())?;
    let mi: i64 = ti.next().ok_or(())?.parse().map_err(|_| ())?;
    let sec: i64 = ti
        .next()
        .map(|p| p.split('.').next().unwrap_or("0").parse().unwrap_or(0))
        .unwrap_or(0);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return Err(());
    }
    Ok((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - off_min * 60) * 1000)
}

fn format_epoch_ms(ms: i64) -> String {
    let mut secs = ms.div_euclid(1000);
    let mins = secs.div_euclid(60);
    secs %= 60;
    let hours = mins.div_euclid(60);
    let mins = mins % 60;
    let days = hours.div_euclid(24);
    let hours = hours % 24;
    // days -> civil date.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{mins:02}:{secs:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_path_from_login_shell_output() {
        let out = b"shell banner\n\0PATH=/home/test/bin:/usr/bin\0HOME=/home/test\0";
        assert_eq!(
            parse_path_from_env0(out).as_deref(),
            Some("/home/test/bin:/usr/bin")
        );
        assert_eq!(parse_path_from_env0(b""), None);
        assert_eq!(parse_path_from_env0(b"\0HOME=x\0"), None);
    }

    #[test]
    fn splits_words() {
        assert_eq!(split_words(""), Vec::<String>::new());
        assert_eq!(split_words("codex app-server --stdio"), vec!["codex", "app-server", "--stdio"]);
        assert_eq!(
            split_words("\"my cli\" --bin 'a b'"),
            vec!["my cli", "--bin", "a b"]
        );
    }

    #[test]
    fn iso_from_epoch_numbers() {
        assert_eq!(iso_timestamp(Some(&json!(0))), "1970-01-01T00:00:00Z");
        assert_eq!(iso_timestamp(Some(&json!(1700000000000i64))), "2023-11-14T22:13:20Z");
        assert_eq!(iso_timestamp(Some(&json!(1700000000i64))), "2023-11-14T22:13:20Z");
        assert_eq!(iso_timestamp(None), "");
        assert_eq!(iso_timestamp(Some(&json!(""))), "");
        assert_eq!(iso_timestamp(Some(&json!("garbage"))), "");
    }

    #[test]
    fn iso_normalizes_strings() {
        assert_eq!(
            iso_timestamp(Some(&json!("2026-01-01T02:00:00+02:00"))),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            iso_timestamp(Some(&json!("2025-12-31T19:00:00-0500"))),
            "2026-01-01T00:00:00Z"
        );
    }

    #[test]
    fn dir_max_mtime_missing_is_zero() {
        assert_eq!(dir_max_mtime("~/.token-cafe-definitely-missing-dir", 100), 0);
    }

    #[test]
    fn dir_max_mtime_finds_newest_file() {
        let base = std::env::temp_dir().join(format!("tc-probe-test-{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("a.txt"), "a").unwrap();
        std::fs::write(sub.join("b.txt"), "b").unwrap();
        let got = dir_max_mtime(base.to_str().unwrap(), 100);
        assert!(got > 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn file_mtime_missing_is_zero() {
        assert_eq!(
            file_mtime_secs(std::path::Path::new("/definitely/missing/tc-probe-test-file")),
            0
        );
    }
}
