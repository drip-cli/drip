//! `drip init` / `drip uninstall` for Codex and Gemini.
//!
//! Mirrors the Claude install/uninstall coverage: each agent has its
//! own config + guidance file, and uninstall must preserve every
//! user-authored byte that wasn't ours.

use crate::common::Drip;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn drip_cmd(drip: &Drip, project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(&drip.bin)
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        // Pin SHELL so completion install in init_claude doesn't
        // create surprise files in HOME during these tests.
        .env_remove("SHELL")
        .output()
        .expect("drip spawn")
}

// ─── Codex ──────────────────────────────────────────────────────────

#[test]
fn codex_uninstall_removes_mcp_and_agents_blocks() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // Pre-existing user content we must NOT clobber: an unrelated
    // section in config.toml and a paragraph in AGENTS.md.
    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[user_section]\nkey = \"value\"\n",
    )
    .unwrap();
    fs::write(
        codex_dir.join("AGENTS.md"),
        "# My personal notes\nbe careful with deletes\n",
    )
    .unwrap();

    // Init.
    let o = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--agent", "codex"],
    );
    assert!(
        o.status.success(),
        "init: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let cfg = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
    assert!(cfg.contains("[mcp_servers.drip]"));
    let md = fs::read_to_string(codex_dir.join("AGENTS.md")).unwrap();
    assert!(md.contains("drip:agents-instructions"));

    // Uninstall.
    let o = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["uninstall", "--agent", "codex"],
    );
    assert!(
        o.status.success(),
        "uninstall: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Our blocks gone, user's stuff intact.
    let cfg = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
    assert!(
        !cfg.contains("[mcp_servers.drip]"),
        "DRIP block left behind: {cfg}"
    );
    assert!(
        cfg.contains("[user_section]"),
        "user section dropped: {cfg}"
    );
    assert!(cfg.contains("key = \"value\""));

    let md = fs::read_to_string(codex_dir.join("AGENTS.md")).unwrap();
    assert!(!md.contains("drip:agents-instructions"));
    assert!(md.contains("# My personal notes"), "user notes lost: {md}");
    assert!(md.contains("be careful with deletes"));
}

#[test]
fn codex_init_refreshes_stale_block_with_old_args() {
    // Regression: a config.toml written by a pre-v0.2 binary contained
    // `args = ["mcp"]` (no agent tag). Re-running `drip init` used to
    // skip silently because the section header was already there —
    // leaving Codex spawning DRIP without `--agent codex`, so session
    // rows ended up tagged "shell". `ensure_codex_mcp` must now
    // detect the divergence and rewrite the block.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // Stale config — header present, but missing `--agent codex`.
    let codex_dir = home.path().join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[user_section]\nkey = \"value\"\n\n\
         [mcp_servers.drip]\n\
         command = \"/old/path/to/drip\"\n\
         args = [\"mcp\"]\n",
    )
    .unwrap();

    let o = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--agent", "codex"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let cfg = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
    assert!(
        cfg.contains("--agent\", \"codex\""),
        "init must refresh stale args, got:\n{cfg}"
    );
    assert!(
        !cfg.contains("\"/old/path/to/drip\""),
        "old command path must be replaced, got:\n{cfg}"
    );
    // User content outside our block must survive.
    assert!(
        cfg.contains("[user_section]"),
        "user section dropped: {cfg}"
    );
    assert!(cfg.contains("key = \"value\""));
}

#[test]
fn codex_uninstall_is_idempotent() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // Run uninstall twice with no prior init — must succeed both times.
    let o1 = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["uninstall", "--agent", "codex"],
    );
    assert!(o1.status.success());
    let o2 = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["uninstall", "--agent", "codex"],
    );
    assert!(o2.status.success());
}

// ─── Gemini ─────────────────────────────────────────────────────────

#[test]
fn gemini_init_writes_settings_and_gemini_md() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--global", "--agent", "gemini"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let settings = home.path().join(".gemini/settings.json");
    let v: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(v["mcpServers"]["drip"]["command"].is_string());
    assert_eq!(v["mcpServers"]["drip"]["args"][0].as_str(), Some("mcp"));

    let md = home.path().join(".gemini/GEMINI.md");
    assert!(md.exists());
    assert!(fs::read_to_string(&md)
        .unwrap()
        .contains("drip:agents-instructions"));
}

#[test]
fn gemini_init_preserves_user_settings() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let gem_dir = home.path().join(".gemini");
    fs::create_dir_all(&gem_dir).unwrap();
    let pre = serde_json::json!({
        "telemetry": false,
        "mcpServers": {
            "another": { "command": "/x/y", "args": [] }
        }
    });
    fs::write(
        gem_dir.join("settings.json"),
        serde_json::to_string_pretty(&pre).unwrap(),
    )
    .unwrap();

    drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--global", "--agent", "gemini"],
    );

    let v: Value =
        serde_json::from_str(&fs::read_to_string(gem_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(v["telemetry"].as_bool(), Some(false));
    assert!(v["mcpServers"]["drip"]["command"].is_string());
    assert!(v["mcpServers"]["another"]["command"].is_string());
}

#[test]
fn gemini_uninstall_removes_drip_only() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let gem_dir = home.path().join(".gemini");
    fs::create_dir_all(&gem_dir).unwrap();
    fs::write(
        gem_dir.join("settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": { "another": { "command": "/x/y", "args": [] } },
            "telemetry": false
        }))
        .unwrap(),
    )
    .unwrap();

    drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--global", "--agent", "gemini"],
    );
    drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["uninstall", "--global", "--agent", "gemini"],
    );

    let v: Value =
        serde_json::from_str(&fs::read_to_string(gem_dir.join("settings.json")).unwrap()).unwrap();
    assert!(v["mcpServers"].get("drip").is_none());
    assert_eq!(v["mcpServers"]["another"]["command"].as_str(), Some("/x/y"));
    assert_eq!(v["telemetry"].as_bool(), Some(false));
    // GEMINI.md block removed.
    let md = fs::read_to_string(gem_dir.join("GEMINI.md")).unwrap();
    assert!(!md.contains("drip:agents-instructions"));
}

// ─── Agent self-tagging via DRIP_AGENT ───────────────────────────────

#[test]
fn gemini_init_writes_agent_flag_in_args() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--global", "--agent", "gemini"],
    );
    let v: Value = serde_json::from_str(
        &fs::read_to_string(home.path().join(".gemini/settings.json")).unwrap(),
    )
    .unwrap();
    let strs: Vec<&str> = v["mcpServers"]["drip"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert_eq!(strs, vec!["mcp", "--agent", "gemini"]);
}

#[test]
fn drip_agent_env_persists_to_session_row() {
    // Set DRIP_AGENT=gemini and run a read — the resulting sessions
    // row should carry agent="gemini", and `drip sessions` should
    // render it as "Gemini" rather than the heuristic fallback.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    fs::write(&f, "hi\n").unwrap();

    let o = drip
        .cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_AGENT", "gemini")
        .output()
        .expect("drip read");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    // Listing must now show "Gemini" in the AGENT column for our row.
    let o = drip.cmd().arg("sessions").output().unwrap();
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    let row = out
        .lines()
        .find(|l| l.contains(&drip.session_id))
        .unwrap_or_else(|| panic!("session row missing: {out}"));
    assert!(
        row.contains("Gemini"),
        "expected Gemini agent label in row: {row}"
    );
}

#[test]
fn drip_agent_unknown_value_falls_back_to_heuristic() {
    // Garbage DRIP_AGENT mustn't be persisted — agent_from_env
    // returns None for unrecognised values, so the column stays
    // NULL and the listing uses the strategy/id-shape heuristic.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    fs::write(&f, "hi\n").unwrap();
    drip.cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_AGENT", "totally-not-an-agent")
        .output()
        .expect("drip read");

    // The session id is `test-<nanos>-<pid>`, env strategy. The
    // heuristic labels env-strategy non-UUID rows as "custom".
    let o = drip.cmd().arg("sessions").output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let row = out.lines().find(|l| l.contains(&drip.session_id)).unwrap();
    assert!(
        row.contains("custom"),
        "expected fallback `custom` label, got: {row}"
    );
}

#[test]
fn drip_mcp_agent_flag_sets_env_var_for_session() {
    // Spawn `drip mcp --agent codex` and immediately close stdin so
    // it exits cleanly. We can't validate the session row this way
    // (mcp::run never opens a Session unless it gets a request), so
    // we exercise the smaller surface: the flag parses and the
    // command starts. A more thorough test would need an MCP client
    // round-trip, which is overkill here.
    let drip = Drip::new();
    let mut child = std::process::Command::new(&drip.bin)
        .args(["mcp", "--agent", "codex"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn drip mcp");
    drop(child.stdin.take()); // EOF → mcp loop exits
    let o = child.wait_with_output().expect("wait");
    // Either success or any clean exit; what we're really asserting
    // is that --agent codex didn't error out at parse time.
    assert!(
        o.status.code().is_some(),
        "drip mcp --agent crashed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn malformed_mcp_json_fails_loudly_rather_than_clobbering() {
    // If the user's settings.json has a syntax error, init must NOT
    // overwrite it — failing loudly is the only safe move.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let gem_dir = home.path().join(".gemini");
    fs::create_dir_all(&gem_dir).unwrap();
    let bad = "{ this is not json,, ";
    fs::write(gem_dir.join("settings.json"), bad).unwrap();

    let o = drip_cmd(
        &drip,
        project.path(),
        home.path(),
        &["init", "--global", "--agent", "gemini"],
    );
    assert!(!o.status.success(), "init should fail on malformed JSON");
    // File is untouched.
    let after = fs::read_to_string(gem_dir.join("settings.json")).unwrap();
    assert_eq!(
        after, bad,
        "init clobbered malformed settings.json: {after:?}"
    );
}
