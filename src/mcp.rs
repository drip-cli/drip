//! Minimal MCP stdio server. Just enough of the spec for any MCP
//! client to call a single `read_file` tool that runs DRIP's delta
//! logic. Wire format: line-delimited JSON-RPC 2.0.

use crate::commands::read;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};

const PROTOCOL_VERSION: &str = "2025-06-18";

const READ_FILE_DESCRIPTION: &str = "\
Read a file with DRIP delta interception. \
On the first read in a session, returns the full file content. \
On every subsequent read of the same file, returns ONLY a unified \
diff against the previously-read version, typically saving 60–95% of \
tokens. \
ALWAYS prefer this tool over native shell `cat`, `head`, `tail`, or \
built-in file-read tools when reading any file you might re-read later.";

/// Cap a single JSON-RPC line so a hostile client can't pin memory.
/// Matches the hook-stdin cap in `commands/hook.rs`.
const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;

pub fn run() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut buf = Vec::with_capacity(4096);
    let mut handle = stdin.lock();

    loop {
        buf.clear();
        // `read_until` over `read_line` so the cap fires before
        // UTF-8 validation allocates the full payload.
        let mut limited = (&mut handle).take(MAX_REQUEST_BYTES + 1);
        let n = limited.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        if n as u64 > MAX_REQUEST_BYTES {
            // Drain to the next `\n` via fill_buf/consume so we don't
            // re-buffer the offending line; cap total drain so a
            // newline-less `/dev/zero` can't hang us.
            let mut drained: u64 = 0;
            let mut found_nl = false;
            while drained < MAX_REQUEST_BYTES {
                let chunk = match handle.fill_buf() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                if chunk.is_empty() {
                    break;
                }
                if let Some(idx) = chunk.iter().position(|&b| b == b'\n') {
                    handle.consume(idx + 1);
                    found_nl = true;
                    break;
                }
                let len = chunk.len();
                drained += len as u64;
                handle.consume(len);
            }
            write_error(&mut out, Value::Null, -32600, "request exceeds 4 MiB cap")?;
            if !found_nl {
                // No clean boundary within cap — bail rather than
                // mis-parse the stream.
                return Ok(());
            }
            continue;
        }
        // `read_until` keeps the trailing `\n`; we still need `&str`.
        // Reject
        // non-UTF-8 explicitly rather than mangling.
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s,
            Err(_) => {
                write_error(&mut out, Value::Null, -32700, "request not valid utf-8")?;
                continue;
            }
        };
        // Re-bind into the existing string-shaped flow.
        let buf = line.to_string();
        let buf = buf.as_str();
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                write_error(&mut out, Value::Null, -32700, "parse error")?;
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // No id (or explicit null) ⇒ notification, no response.
        let is_notification = matches!(&id, None | Some(Value::Null));
        if is_notification {
            continue;
        }
        let id = id.unwrap();

        match method {
            "initialize" => {
                write_result(&mut out, id, initialize_result())?;
            }
            "tools/list" => {
                write_result(&mut out, id, tools_list_result())?;
            }
            "tools/call" => {
                let params = req.get("params");
                match tools_call(params) {
                    Ok(v) => write_result(&mut out, id, v)?,
                    Err(msg) => write_error(&mut out, id, -32602, &msg)?,
                }
            }
            "ping" => {
                write_result(&mut out, id, json!({}))?;
            }
            _ => {
                write_error(&mut out, id, -32601, "method not found")?;
            }
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "drip",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "read_file",
                "description": READ_FILE_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Absolute or working-directory-relative path to the file."
                        }
                    },
                    "required": ["file_path"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn tools_call(params: Option<&Value>) -> std::result::Result<Value, String> {
    let p = params.ok_or_else(|| "missing params".to_string())?;
    let name = p
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing tool name".to_string())?;
    if name != "read_file" {
        return Err(format!("unknown tool: {name}"));
    }
    let args = p
        .get("arguments")
        .ok_or_else(|| "missing arguments".to_string())?;
    let file = args
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "file_path required".to_string())?;

    if let Some(reason) = workspace_violation(file) {
        return Ok(json!({
            "content": [{ "type": "text", "text": reason }],
            "isError": true
        }));
    }

    // Honour the universal kill switch: when DRIP_DISABLE is set, the
    // MCP `read_file` tool degrades to a plain raw-content read so the
    // calling agent still sees the file but without the delta logic
    // — same contract as the Claude hooks. Workspace boundary checks
    // still apply.
    if std::env::var_os("DRIP_DISABLE").is_some() {
        let resolved = crate::core::session::resolve_path(file);
        return match std::fs::read_to_string(&resolved) {
            Ok(text) => Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            })),
            Err(e) => Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!("DRIP error (DRIP_DISABLE bypass): {e}")
                }],
                "isError": true
            })),
        };
    }

    match read::run(file) {
        Ok(text) => Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        })),
        Err(e) => Ok(json!({
            "content": [{ "type": "text", "text": format!("DRIP error: {e}") }],
            "isError": true
        })),
    }
}

/// Defense in depth for MCP clients: when `DRIP_WORKSPACE_ROOT` is set,
/// refuse any read whose canonicalized path falls outside that root.
/// Returns `Some(reason)` to refuse, `None` to allow.
fn workspace_violation(file: &str) -> Option<String> {
    let root = std::env::var_os("DRIP_WORKSPACE_ROOT")?;
    let root_path = std::path::Path::new(&root);
    let root_canon = root_path.canonicalize().ok()?;

    let target = std::path::Path::new(file);
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(target)
    };
    // Fail closed when canonicalization fails (e.g. missing file, broken
    // symlink, permission denied). Path::starts_with on a non-canonical
    // path is purely textual — `/root/../../etc/passwd` would falsely
    // appear to start with `/root`. Refusing here is safe: a legitimate
    // read of a real file will canonicalize successfully.
    let target_canon = match resolved.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            return Some(format!(
                "DRIP refused read: cannot resolve {} (file missing or outside workspace)",
                resolved.display()
            ));
        }
    };

    if target_canon.starts_with(&root_canon) {
        None
    } else {
        Some(format!(
            "DRIP refused read: {} is outside DRIP_WORKSPACE_ROOT ({})",
            target_canon.display(),
            root_canon.display()
        ))
    }
}

fn write_result<W: Write>(w: &mut W, id: Value, result: Value) -> Result<()> {
    let env = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    writeln!(w, "{env}")?;
    w.flush()?;
    Ok(())
}

fn write_error<W: Write>(w: &mut W, id: Value, code: i32, msg: &str) -> Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": msg }
    });
    writeln!(w, "{env}")?;
    w.flush()?;
    Ok(())
}
