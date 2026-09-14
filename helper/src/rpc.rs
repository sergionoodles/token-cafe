//! One-shot JSON-RPC 2.0 client over a child's stdio pipes.
//! Mirrors the old Python `JsonLineProcess`: line-delimited JSON, responses
//! matched by id, server-initiated requests answered "method not supported".

use crate::util::{remaining, ProbeError, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct JsonLineProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Option<Value>>,
    _reader: std::thread::JoinHandle<()>,
}

impl JsonLineProcess {
    pub fn spawn(argv: &[String]) -> Result<Self> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| ProbeError(format!("Could not start CLI: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| ProbeError("No stdin".to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| ProbeError("No stdout".to_string()))?;
        let mut stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            // Drain stderr so a chatty child never blocks.
            if let Some(mut err) = stderr.take() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    use std::io::Read;
                    while err.read(&mut buf).map(|n| n).unwrap_or(0) > 0 {}
                });
            }
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next() {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(Some(v)).is_err() {
                        break;
                    }
                }
                // Skip non-JSON noise lines (same as the old probe).
            }
            let _ = tx.send(None);
        });
        Ok(Self {
            child,
            stdin,
            lines: rx,
            _reader: reader,
        })
    }

    fn exited_status(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.code().unwrap_or(-1))
    }

    pub fn send(&mut self, msg: &Value) -> Result<()> {
        if let Some(code) = self.exited_status() {
            return Err(ProbeError(format!("CLI exited with status {code}")));
        }
        let mut payload = serde_json::to_string(msg).map_err(ProbeError::from)?;
        payload.push('\n');
        self.stdin
            .write_all(payload.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|_| ProbeError("CLI closed its input unexpectedly".to_string()))
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            let timeout = remaining(deadline).map_err(|_| ProbeError("CLI protocol timed out".to_string()))?;
            match self.lines.recv_timeout(timeout) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => {
                    let code = self.exited_status().unwrap_or(-1);
                    return Err(ProbeError(format!(
                        "CLI protocol closed unexpectedly (status {code})"
                    )));
                }
                Err(_) => return Err(ProbeError("CLI protocol timed out".to_string())),
            }
        }
    }

    pub fn request(&mut self, id: i64, method: &str, params: Option<Value>, deadline: Instant) -> Result<Value> {
        let mut msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send(&msg)?;
        loop {
            remaining(deadline).map_err(|_| ProbeError("CLI protocol timed out".to_string()))?;
            let resp = self.receive(deadline)?;
            let rid = resp.get("id").and_then(|v| v.as_i64());
            let has_method = resp.get("method").and_then(|v| v.as_str()).is_some();
            if rid == Some(id) && !has_method {
                if let Some(err) = resp.get("error") {
                    let detail = err
                        .get("data")
                        .or_else(|| err.get("message"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| err.to_string());
                    return Err(ProbeError(format!("{method}: {detail}")));
                }
                return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
            }
            if rid.is_some() && has_method {
                // Server-initiated request: answer "not supported".
                let reply = serde_json::json!({
                    "jsonrpc": "2.0", "id": rid,
                    "error": {"code": -32601, "message": "Method not supported"},
                });
                self.send(&reply)?;
            }
        }
    }
}

impl Drop for JsonLineProcess {
    fn drop(&mut self) {
        let pid = self.child.id() as i32;
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
        }
        let start = Instant::now();
        while self.child.try_wait().map(|s| s.is_none()).unwrap_or(false)
            && start.elapsed() < Duration::from_secs(1)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }
}
