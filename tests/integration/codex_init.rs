use crate::common::Drip;
use std::fs;
use std::process::Command;

fn run_init(drip: &Drip, home: &std::path::Path) -> std::process::Output {
    Command::new(&drip.bin)
        .arg("init")
        .arg("--agent")
        .arg("codex")
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip init codex")
}

#[test]
fn codex_init_writes_mcp_config_and_agents_md() {
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, home.path());
    assert!(
        o.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let cfg = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    assert!(cfg.contains("[mcp_servers.drip]"), "config.toml: {cfg}");
    // Args now self-tag the agent so `drip sessions` can distinguish
    // Codex from a plain shell (DRIP_AGENT=codex via --agent flag).
    assert!(
        cfg.contains("args = [\"mcp\", \"--agent\", \"codex\"]"),
        "expected --agent codex tag in args: {cfg}"
    );

    let agents = fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();
    assert!(agents.contains("File reads via DRIP"));
    assert!(agents.contains("read_file"));
}

#[test]
fn codex_init_is_idempotent() {
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();

    run_init(&drip, home.path());
    let cfg1 = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    let agents1 = fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();

    run_init(&drip, home.path());
    let cfg2 = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    let agents2 = fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap();

    assert_eq!(cfg1, cfg2, "config.toml drifted on second init");
    assert_eq!(agents1, agents2, "AGENTS.md drifted on second init");
}

#[test]
fn codex_init_preserves_existing_user_config() {
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    let user_cfg = "[some_other]\nkey = \"value\"\n";
    fs::write(home.path().join(".codex/config.toml"), user_cfg).unwrap();

    let o = run_init(&drip, home.path());
    assert!(o.status.success());

    let cfg = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    assert!(cfg.contains("[some_other]"), "user section dropped: {cfg}");
    assert!(cfg.contains("key = \"value\""));
    assert!(cfg.contains("[mcp_servers.drip]"));
}
