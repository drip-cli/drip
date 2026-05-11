//! `drip completions` — shell completion script generation.
//!
//! Verifies each supported shell emits a non-empty, syntactically
//! plausible script and that the major subcommands are present.
//! Also covers init/uninstall side-effects on the conventional
//! completion paths.

use crate::common::Drip;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(drip: &Drip, args: &[&str]) -> Output {
    Command::new(&drip.bin)
        .args(args)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip spawn")
}

fn run_init(drip: &Drip, project: &Path, home: &Path, shell: &str) -> Output {
    Command::new(&drip.bin)
        .args(["init", "--global", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("SHELL", shell)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip init")
}

fn run_uninstall(drip: &Drip, project: &Path, home: &Path, shell: &str) -> Output {
    Command::new(&drip.bin)
        .args(["uninstall", "--global", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("SHELL", shell)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip uninstall")
}

#[test]
fn completions_bash_nonempty() {
    let drip = Drip::new();
    let o = run(&drip, &["completions", "bash"]);
    assert!(
        o.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.len() > 200, "bash script too small ({} bytes)", s.len());
    // clap_complete bash output declares the function `_drip` and
    // registers it via `complete`.
    assert!(
        s.contains("_drip"),
        "bash script missing _drip function: {s}"
    );
    assert!(s.contains("complete"), "bash script missing complete call");
}

#[test]
fn completions_zsh_nonempty() {
    let drip = Drip::new();
    let o = run(&drip, &["completions", "zsh"]);
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    // zsh completion files start with `#compdef <name>`.
    assert!(
        s.starts_with("#compdef drip"),
        "zsh script must begin with #compdef drip, got: {}",
        s.lines().next().unwrap_or("")
    );
}

#[test]
fn completions_fish_nonempty() {
    let drip = Drip::new();
    let o = run(&drip, &["completions", "fish"]);
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    // Fish completions are a series of `complete -c drip ...` lines.
    assert!(
        s.contains("complete -c drip"),
        "fish script missing `complete -c drip`: {s}"
    );
}

#[test]
fn completions_unknown_shell_error() {
    let drip = Drip::new();
    // Stick with a shell we genuinely don't support — tcsh / ksh are
    // safe choices since clap_complete itself doesn't ship them.
    let o = run(&drip, &["completions", "tcsh"]);
    assert!(!o.status.success(), "expected failure for unknown shell");
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.to_lowercase().contains("unsupported") || err.to_lowercase().contains("invalid"),
        "stderr should explain: {err}"
    );
}

#[test]
fn completions_powershell_nonempty() {
    let drip = Drip::new();
    let o = run(&drip, &["completions", "powershell"]);
    assert!(
        o.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let s = String::from_utf8_lossy(&o.stdout);
    // Cross-platform: PowerShell scripts work on Windows natively
    // and via `pwsh` on macOS/Linux, so the test is sane everywhere.
    assert!(
        s.len() > 200,
        "powershell script too small ({} bytes)",
        s.len()
    );
    assert!(s.contains("drip"), "powershell script must reference drip");
    // `pwsh` accepts both spellings.
    let o2 = run(&drip, &["completions", "pwsh"]);
    assert!(o2.status.success(), "pwsh alias failed");
}

#[test]
fn completions_elvish_nonempty() {
    let drip = Drip::new();
    let o = run(&drip, &["completions", "elvish"]);
    assert!(
        o.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("drip"), "elvish script must reference drip");
}

#[test]
fn init_skips_powershell_auto_install() {
    // SHELL=/usr/bin/pwsh shouldn't tempt drip into dropping a file
    // somewhere — PowerShell uses dot-sourcing from $PROFILE, not a
    // search-path drop-in. Init must succeed AND not write anything
    // PowerShell-related under HOME.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, project.path(), home.path(), "/usr/bin/pwsh");
    assert!(
        o.status.success(),
        "init failed under SHELL=pwsh: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    // No completion files at any of the known paths.
    assert!(!home.path().join(".zsh/completions/_drip").exists());
    assert!(!home.path().join(".bash_completion.d/drip.bash").exists());
    assert!(!home
        .path()
        .join(".config/fish/completions/drip.fish")
        .exists());
}

#[test]
fn completions_contain_subcommands() {
    // The generated scripts should mention every user-facing
    // subcommand — that's the whole point of completions.
    let drip = Drip::new();
    let expected = [
        "read",
        "init",
        "uninstall",
        "meter",
        "reset",
        "refresh",
        "sessions",
        "cache",
        "watch",
        "replay",
        "doctor",
        "completions",
    ];
    for shell in ["bash", "zsh", "fish"] {
        let o = run(&drip, &["completions", shell]);
        assert!(o.status.success(), "{shell} failed");
        let s = String::from_utf8_lossy(&o.stdout);
        for sub in expected {
            assert!(s.contains(sub), "{shell} script missing subcommand `{sub}`");
        }
    }
}

#[test]
fn init_installs_zsh_completion_file() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, project.path(), home.path(), "/bin/zsh");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let comp = home.path().join(".zsh/completions/_drip");
    assert!(
        comp.exists(),
        "zsh completion file not installed at {comp:?}"
    );
    let body = fs::read_to_string(&comp).unwrap();
    assert!(body.starts_with("#compdef drip"), "wrong contents: {body}");
    // init's stdout report mentions the install.
    let report = String::from_utf8_lossy(&o.stdout);
    assert!(
        report.contains("zsh") && report.contains("_drip"),
        "init didn't report completion install: {report}"
    );
}

#[test]
fn init_installs_bash_completion_file() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, project.path(), home.path(), "/bin/bash");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let comp = home.path().join(".bash_completion.d/drip.bash");
    assert!(comp.exists(), "bash completion file not installed");
    assert!(fs::read_to_string(&comp).unwrap().contains("_drip"));
}

#[test]
fn init_installs_fish_completion_file() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, project.path(), home.path(), "/usr/local/bin/fish");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let comp = home.path().join(".config/fish/completions/drip.fish");
    assert!(comp.exists(), "fish completion file not installed");
    assert!(fs::read_to_string(&comp)
        .unwrap()
        .contains("complete -c drip"));
}

#[test]
fn init_unknown_shell_skips_silently() {
    // No completions for tcsh — but init must NOT fail.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init(&drip, project.path(), home.path(), "/bin/tcsh");
    assert!(
        o.status.success(),
        "init failed on unknown shell: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    // No completion files were created.
    assert!(!home.path().join(".zsh/completions/_drip").exists());
    assert!(!home.path().join(".bash_completion.d/drip.bash").exists());
}

#[test]
fn uninstall_removes_zsh_completion() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    run_init(&drip, project.path(), home.path(), "/bin/zsh");
    let comp = home.path().join(".zsh/completions/_drip");
    assert!(comp.exists(), "init didn't install");

    let o = run_uninstall(&drip, project.path(), home.path(), "/bin/zsh");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        !comp.exists(),
        "completion file still present after uninstall"
    );
}

#[test]
fn uninstall_preserves_user_authored_completion() {
    // If a file already exists at the conventional path but isn't
    // ours (no `#compdef drip` header), uninstall must NOT remove it.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let dir = home.path().join(".zsh/completions");
    fs::create_dir_all(&dir).unwrap();
    let user_file = dir.join("_drip");
    fs::write(&user_file, "# my own custom completion\nfoo bar\n").unwrap();

    let o = run_uninstall(&drip, project.path(), home.path(), "/bin/zsh");
    assert!(o.status.success());
    assert!(user_file.exists(), "user-authored completion was removed");
    let body = fs::read_to_string(&user_file).unwrap();
    assert!(body.contains("my own custom completion"));
}
