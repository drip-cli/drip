use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn line(req: Value) -> String {
    let mut s = req.to_string();
    s.push('\n');
    s
}

fn read_one(reader: &mut BufReader<&mut std::process::ChildStdout>) -> Value {
    let mut buf = String::new();
    let n = reader.read_line(&mut buf).expect("read mcp line");
    assert!(n > 0, "MCP server closed stdout unexpectedly");
    serde_json::from_str(buf.trim()).expect("JSON-RPC line")
}

fn spawn_mcp(drip: &Drip) -> std::process::Child {
    Command::new(&drip.bin)
        .arg("mcp")
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn drip mcp")
}

#[test]
fn mcp_initialize_lists_read_file_tool() {
    let drip = Drip::new();
    let mut child = spawn_mcp(&drip);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);

    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            }))
            .as_bytes(),
        )
        .unwrap();
    let init = read_one(&mut reader);
    assert_eq!(init["id"], json!(1));
    assert!(init["result"]["protocolVersion"].is_string());
    assert_eq!(init["result"]["serverInfo"]["name"], json!("drip"));

    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }))
            .as_bytes(),
        )
        .unwrap();
    let list = read_one(&mut reader);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|t| t["name"] == "read_file"));

    drop(stdin);
    let _ = child.wait();
}

/// Sec audit (M-2): a JSON-RPC line larger than the 4 MiB cap must
/// not OOM the server. The server should respond with a JSON-RPC
/// error and stay alive for the next request.
#[test]
fn mcp_rejects_oversized_request_line() {
    let drip = Drip::new();
    let mut child = spawn_mcp(&drip);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);

    // Build a JSON-RPC line a hair over 4 MiB. Padding goes into a
    // string field so the JSON itself is well-formed if the server
    // ever did parse it (it shouldn't).
    let pad = "x".repeat(4 * 1024 * 1024 + 16);
    let oversized = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{{\"pad\":\"{pad}\"}}}}\n"
    );
    stdin.write_all(oversized.as_bytes()).unwrap();

    let resp = read_one(&mut reader);
    assert_eq!(resp["jsonrpc"], json!("2.0"));
    assert!(
        resp["error"].is_object(),
        "oversize line should produce a JSON-RPC error, got: {resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("4 MiB cap") || msg.contains("exceeds"),
        "error message should mention the cap, got: {msg}"
    );

    // Server is still alive: a normal `initialize` request after the
    // bad one must succeed.
    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "initialize",
                "params": {}
            }))
            .as_bytes(),
        )
        .unwrap();
    let init = read_one(&mut reader);
    assert_eq!(init["id"], json!(99));
    assert!(
        init["result"]["protocolVersion"].is_string(),
        "server should still serve requests after oversize bail: {init}"
    );

    drop(stdin);
    let _ = child.wait();
}

#[test]
fn mcp_tools_call_returns_full_then_delta() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("mcp.txt");
    // Big enough that a 1-line diff is cheaper than a full re-read.
    let mut v1 = String::new();
    for i in 0..40 {
        v1.push_str(&format!("filler line {i}\n"));
    }
    v1.push_str("alpha\nbeta\ngamma\n");
    fs::write(&f, &v1).unwrap();

    let mut child = spawn_mcp(&drip);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);

    // initialize
    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize"
            }))
            .as_bytes(),
        )
        .unwrap();
    let _ = read_one(&mut reader);

    // first call → full
    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "read_file",
                    "arguments": { "file_path": f.to_string_lossy() }
                }
            }))
            .as_bytes(),
        )
        .unwrap();
    let r1 = read_one(&mut reader);
    let text1 = r1["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        text1.contains("[DRIP: full read"),
        "first response: {text1}"
    );

    // mutate file: same fillers, beta → BETA
    let v2 = v1.replace("beta", "BETA");
    fs::write(&f, &v2).unwrap();

    // second call → delta
    stdin
        .write_all(
            line(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "read_file",
                    "arguments": { "file_path": f.to_string_lossy() }
                }
            }))
            .as_bytes(),
        )
        .unwrap();
    let r2 = read_one(&mut reader);
    let text2 = r2["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        text2.contains("[DRIP: delta only"),
        "second response: {text2}"
    );
    assert!(text2.contains("-beta"));
    assert!(text2.contains("+BETA"));

    drop(stdin);
    let _ = child.wait();
}
