//! `drip update` — detect the install method and run the matching
//! upgrade command. Shells out to `curl`/`wget` rather than pulling
//! in a TLS dependency for one GET request per upgrade.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;

const REPO: &str = "drip-cli/drip";
// Homebrew tap name — `brew tap drip-cli/drip` resolves to the
// `drip-cli/homebrew-drip` repo (Homebrew adds the `homebrew-`
// prefix automatically; the tap name passed to brew is the
// short form). Distinct repos: `drip-cli/drip` hosts source +
// release tarballs, `drip-cli/homebrew-drip` hosts the formula.
const TAP: &str = "drip-cli/drip";
const INSTALL_SCRIPT_URL: &str = "https://raw.githubusercontent.com/drip-cli/drip/main/install.sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Cargo,
    InstallScript,
    Unknown,
}

pub fn run(dry_run: bool) -> Result<()> {
    println!("Checking for updates...");

    let current = env!("CARGO_PKG_VERSION");
    let latest = fetch_latest_version().context("fetching latest version from GitHub")?;

    println!("  Current version : {current}");
    println!("  Latest version  : {latest}");

    if version_compare(current, &latest).is_ge() {
        println!();
        println!("drip {current} is already the latest version.");
        return Ok(());
    }

    println!();
    println!("Updating drip {current} → {latest}...");

    let method = detect_install_method();
    print_method(method);

    if dry_run {
        println!();
        println!("(dry-run — no command executed)");
        return Ok(());
    }

    run_update(method)?;

    println!();
    println!("  ✅  drip {latest} installed successfully.");
    println!("  Run `drip --version` to confirm.");
    Ok(())
}

/// `Some(latest)` when an update is available; `None` when up-to-date.
pub fn check_for_update() -> Result<Option<String>> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = fetch_latest_version()?;
    if version_compare(current, &latest).is_lt() {
        Ok(Some(latest))
    } else {
        Ok(None)
    }
}

/// `GET /repos/<REPO>/releases/latest`, return the tag minus `v`.
/// `DRIP_UPDATE_FAKE_LATEST` short-circuits the network call (tests).
fn fetch_latest_version() -> Result<String> {
    if let Ok(fake) = std::env::var("DRIP_UPDATE_FAKE_LATEST") {
        if !fake.is_empty() {
            return Ok(fake.trim_start_matches('v').to_string());
        }
    }

    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = http_get(&url).context("HTTP GET failed")?;
    let v: serde_json::Value = serde_json::from_str(&body).context("parsing GitHub JSON")?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("no tag_name in GitHub response"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

fn http_get(url: &str) -> Result<String> {
    // Try curl first (universal on macOS/Linux/modern Windows). Fall
    // back to wget for the minority that lacks curl. When BOTH fail
    // we surface what we actually saw from each — the previous
    // single-message error blamed "neither curl nor wget" even when
    // curl had run and returned a real HTTP/network error.
    let curl_diag = match Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "User-Agent: drip-update-check",
            "-H",
            "Accept: application/vnd.github+json",
            "--max-time",
            "10",
            url,
        ])
        .output()
    {
        Ok(o) if o.status.success() => {
            return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
        }
        Ok(o) => format!(
            "curl exited with status {} (stderr: {})",
            o.status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            short_stderr(&o.stderr),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "curl not found in PATH".to_string(),
        Err(e) => format!("curl spawn failed: {e}"),
    };

    match Command::new("wget")
        .args(["-qO-", "--timeout=10", url])
        .output()
    {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => bail!(
            "both fetchers failed — {curl_diag}; wget exited with status {} (stderr: {})",
            o.status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            short_stderr(&o.stderr),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{curl_diag}, and wget is not installed either — install curl or wget and retry")
        }
        Err(e) => bail!("{curl_diag}; wget spawn failed: {e}"),
    }
}

/// Truncate a captured stderr to a single readable line. Keeps error
/// messages snappy when the underlying tool emits a multi-line dump.
fn short_stderr(buf: &[u8]) -> String {
    let s = String::from_utf8_lossy(buf);
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "<empty>".to_string()
    } else if line.len() > 200 {
        format!("{}…", &line[..200])
    } else {
        line.to_string()
    }
}

/// Path-based heuristic: Homebrew (`/homebrew/`, `/Cellar/`,
/// `/linuxbrew/`), cargo (`/.cargo/bin/`), install script
/// (`/.local/bin/`).
pub fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe().unwrap_or_default();
    detect_from_path(&exe)
}

fn detect_from_path(p: &Path) -> InstallMethod {
    let s = p.to_string_lossy();
    if s.contains("/homebrew/") || s.contains("/Cellar/") || s.contains("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if s.contains("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    if s.contains("/.local/bin/") {
        return InstallMethod::InstallScript;
    }
    InstallMethod::Unknown
}

fn print_method(m: InstallMethod) {
    match m {
        InstallMethod::Homebrew => {
            println!("  Detected: Homebrew");
            println!("  Running:  brew upgrade {TAP}/drip");
        }
        InstallMethod::Cargo => {
            println!("  Detected: cargo install");
            println!("  Running:  cargo install drip-cli --force");
        }
        InstallMethod::InstallScript => {
            println!("  Detected: curl install script");
            println!("  Running:  curl -fsSL {INSTALL_SCRIPT_URL} | sh");
        }
        InstallMethod::Unknown => {
            println!("  Detected: unknown install method");
        }
    }
}

fn run_update(m: InstallMethod) -> Result<()> {
    match m {
        InstallMethod::Homebrew => {
            let st = Command::new("brew")
                .args(["upgrade", &format!("{TAP}/drip")])
                .status()
                .context("spawning brew")?;
            if !st.success() {
                bail!("brew upgrade failed (exit {st})");
            }
        }
        InstallMethod::Cargo => {
            let st = Command::new("cargo")
                .args(["install", "drip-cli", "--force"])
                .status()
                .context("spawning cargo")?;
            if !st.success() {
                bail!("cargo install failed (exit {st})");
            }
        }
        InstallMethod::InstallScript => {
            let st = Command::new("sh")
                .args(["-c", &format!("curl -fsSL {INSTALL_SCRIPT_URL} | sh")])
                .status()
                .context("spawning sh")?;
            if !st.success() {
                bail!("install script failed (exit {st})");
            }
        }
        InstallMethod::Unknown => bail!(
            "Cannot detect install method.\n\
             Please update manually:\n  \
             • Homebrew : brew upgrade {TAP}/drip\n  \
             • cargo    : cargo install drip-cli --force\n  \
             • script   : curl -fsSL {INSTALL_SCRIPT_URL} | sh"
        ),
    }
    Ok(())
}

/// `MAJOR.MINOR.PATCH` comparison, ignoring `-suffix` (alpha/rc).
fn version_compare(current: &str, latest: &str) -> std::cmp::Ordering {
    let cur = parse_semver(current);
    let lat = parse_semver(latest);
    cur.cmp(&lat)
}

fn parse_semver(v: &str) -> (u64, u64, u64) {
    let core = v.split('-').next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn detect_homebrew_macos_arm() {
        let p = PathBuf::from("/opt/homebrew/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detect_homebrew_macos_intel() {
        let p = PathBuf::from("/usr/local/Cellar/drip/0.1.0/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detect_homebrew_linuxbrew() {
        let p = PathBuf::from("/home/linuxbrew/.linuxbrew/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::Homebrew);
    }

    #[test]
    fn detect_cargo_install() {
        let p = PathBuf::from("/Users/me/.cargo/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::Cargo);
    }

    #[test]
    fn detect_install_script() {
        let p = PathBuf::from("/home/me/.local/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::InstallScript);
    }

    #[test]
    fn detect_unknown() {
        let p = PathBuf::from("/usr/bin/drip");
        assert_eq!(detect_from_path(&p), InstallMethod::Unknown);
    }

    #[test]
    fn semver_strict_ordering() {
        assert!(version_compare("0.0.9", "0.1.0").is_lt());
        assert!(version_compare("0.1.0", "0.1.0").is_eq());
        assert!(version_compare("0.2.0", "0.1.0").is_gt());
        assert!(version_compare("1.0.0", "0.99.99").is_gt());
    }

    #[test]
    fn semver_handles_prerelease_suffix() {
        assert!(version_compare("0.1.0-rc.1", "0.1.0").is_eq());
    }

    #[test]
    fn semver_handles_garbage_components() {
        assert!(version_compare("oops", "0.1.0").is_lt());
    }

    #[test]
    fn fake_latest_env_var_short_circuits_network() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("DRIP_UPDATE_FAKE_LATEST", "9.9.9");
        let v = fetch_latest_version().unwrap();
        assert_eq!(v, "9.9.9");
        std::env::remove_var("DRIP_UPDATE_FAKE_LATEST");
    }

    #[test]
    fn fake_latest_strips_leading_v() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("DRIP_UPDATE_FAKE_LATEST", "v1.2.3");
        let v = fetch_latest_version().unwrap();
        assert_eq!(v, "1.2.3");
        std::env::remove_var("DRIP_UPDATE_FAKE_LATEST");
    }

    #[test]
    fn short_stderr_handles_empty_input() {
        assert_eq!(short_stderr(b""), "<empty>");
        assert_eq!(short_stderr(b"\n\n"), "<empty>");
    }

    #[test]
    fn short_stderr_returns_first_line() {
        let buf = b"curl: (28) Operation timed out\nadditional debug noise\nmore lines";
        assert_eq!(short_stderr(buf), "curl: (28) Operation timed out");
    }

    #[test]
    fn short_stderr_truncates_overly_long_first_line() {
        let long = "x".repeat(500);
        let out = short_stderr(long.as_bytes());
        assert!(out.ends_with('…'));
        // The truncated payload sits well under the original length.
        assert!(out.len() < 250);
    }

    #[test]
    fn http_get_surfaces_both_diagnostics_when_neither_tool_is_in_path() {
        // Drop curl + wget from PATH and call http_get against a
        // bogus URL. We don't care about the URL since both binaries
        // should fail to spawn — what we care about is that the
        // surfaced error names BOTH "curl" and "wget" (the old code
        // only mentioned the fallback's failure).
        let _guard = ENV_LOCK.lock().unwrap();
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "/var/empty");

        let err = http_get("http://127.0.0.1:1/nope").unwrap_err();
        let msg = format!("{err:#}");

        // Restore PATH before any assertion so a failure doesn't
        // leak a broken env into sibling tests.
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert!(
            msg.contains("curl") && msg.contains("wget"),
            "error must mention both fetchers: {msg}"
        );
        assert!(
            msg.contains("not found") || msg.contains("not installed"),
            "error must say what's missing, not just blame both: {msg}"
        );
    }
}
