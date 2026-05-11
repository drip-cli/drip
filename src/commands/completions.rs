//! `drip completions <shell>` — emit a shell completion script on
//! stdout, plus install/uninstall helpers used by `drip init` and
//! `drip uninstall` to manage the conventional completion files.

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::{generate, shells};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bash/Zsh/Fish auto-install via `drip init` (conventional drop-in
/// directories). PowerShell/Elvish dot-source from `$PROFILE`/`rc.elv`,
/// so we emit on stdout only — `relative_path()` and `fingerprint()`
/// return `None` for those, no-op'ing the auto-install path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl Shell {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "powershell" | "pwsh" => Some(Shell::PowerShell),
            "elvish" => Some(Shell::Elvish),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
            Shell::Elvish => "elvish",
        }
    }

    /// Path under `$HOME` for the auto-installer. `None` for shells
    /// that dot-source from a profile.
    pub fn relative_path(self) -> Option<&'static str> {
        match self {
            Shell::Zsh => Some(".zsh/completions/_drip"),
            Shell::Bash => Some(".bash_completion.d/drip.bash"),
            Shell::Fish => Some(".config/fish/completions/drip.fish"),
            Shell::PowerShell | Shell::Elvish => None,
        }
    }

    /// Map `$SHELL` (a path) to a known shell. PowerShell/Elvish
    /// return `None` — those don't reliably set `$SHELL`.
    pub fn from_shell_var(value: &str) -> Option<Self> {
        let last = Path::new(value).file_name().and_then(|n| n.to_str())?;
        match Self::parse(last)? {
            s @ (Shell::Bash | Shell::Zsh | Shell::Fish) => Some(s),
            _ => None,
        }
    }

    /// Distinguishing fingerprint so `uninstall` won't delete a
    /// user-authored completion that happens to share our path.
    fn fingerprint(self) -> Option<&'static str> {
        match self {
            Shell::Zsh => Some("#compdef drip"),
            Shell::Bash => Some("_drip()"),
            Shell::Fish => Some("complete -c drip"),
            Shell::PowerShell | Shell::Elvish => None,
        }
    }

    /// Multi-line snippet the user must add to their shell rc to
    /// actually pick up the completion. Zsh is the painful case:
    /// dropping `_drip` into `~/.zsh/completions` does nothing unless
    /// that directory is on `$fpath` AND `compinit` runs. Bash needs
    /// to source the file. Fish autoloads
    /// `~/.config/fish/completions/` out of the box. PowerShell &
    /// Elvish dot-source via their respective rc files.
    pub fn activation_hint(self) -> &'static str {
        match self {
            Shell::Zsh => {
                "\
Add to ~/.zshrc (once), then `source ~/.zshrc`:
    fpath=(~/.zsh/completions $fpath)
    autoload -Uz compinit && compinit"
            }
            Shell::Bash => {
                "\
Add to ~/.bashrc (once), then `source ~/.bashrc`:
    [ -f ~/.bash_completion.d/drip.bash ] && source ~/.bash_completion.d/drip.bash"
            }
            Shell::Fish => {
                "\
Open a new fish session — `~/.config/fish/completions/` is autoloaded."
            }
            Shell::PowerShell => {
                "\
Add to your PowerShell profile (`echo $PROFILE`), then start a new shell:
    drip completions powershell | Out-String | Invoke-Expression"
            }
            Shell::Elvish => {
                "\
Add to ~/.config/elvish/rc.elv, then start a new shell:
    eval (drip completions elvish | slurp)"
            }
        }
    }
}

/// CLI entry point: emit the script for `shell` to stdout.
pub fn run(shell: &str) -> Result<()> {
    let s = Shell::parse(shell)
        .with_context(|| format!("unsupported shell: {shell}. Supported: bash, zsh, fish"))?;
    let mut out = std::io::stdout().lock();
    write_script(s, &mut out)?;
    Ok(())
}

fn write_script(shell: Shell, w: &mut impl Write) -> Result<()> {
    let mut cmd = crate::Cli::command();
    match shell {
        Shell::Bash => generate(shells::Bash, &mut cmd, "drip", w),
        Shell::Zsh => generate(shells::Zsh, &mut cmd, "drip", w),
        Shell::Fish => generate(shells::Fish, &mut cmd, "drip", w),
        Shell::PowerShell => generate(shells::PowerShell, &mut cmd, "drip", w),
        Shell::Elvish => generate(shells::Elvish, &mut cmd, "drip", w),
    }
    Ok(())
}

/// Generate the script as a `Vec<u8>` (used by the install path).
fn generate_to_buf(shell: Shell) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = write_script(shell, &mut buf);
    buf
}

/// Detect the user's shell from `$SHELL`. `None` for unknown shells.
pub fn detect_shell() -> Option<Shell> {
    let v = std::env::var("SHELL").ok()?;
    Shell::from_shell_var(&v)
}

/// `~/<rel>`. `None` for shells without an auto-install path.
fn home_path(shell: Shell) -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(shell.relative_path()?))
}

/// Install at the detected shell's conventional path.
/// `Ok(None)` for unknown / unsupported shells so `drip init` skips
/// silently.
pub fn install_for_detected_shell() -> Result<Option<PathBuf>> {
    let Some(shell) = detect_shell() else {
        return Ok(None);
    };
    let Some(target) = home_path(shell) else {
        return Ok(None);
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    let bytes = generate_to_buf(shell);
    std::fs::write(&target, &bytes).with_context(|| format!("writing {target:?}"))?;
    Ok(Some(target))
}

/// Remove the completion only when its content matches our
/// fingerprint — never stomp on a user-authored file at the same path.
pub fn uninstall_for_detected_shell() -> Result<Option<PathBuf>> {
    let Some(shell) = detect_shell() else {
        return Ok(None);
    };
    let Some(target) = home_path(shell) else {
        return Ok(None);
    };
    if !target.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&target).with_context(|| format!("reading {target:?}"))?;
    let Some(fp) = shell.fingerprint() else {
        return Ok(None);
    };
    if !body.contains(fp) {
        return Ok(None);
    }
    std::fs::remove_file(&target).with_context(|| format!("removing {target:?}"))?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_shells() {
        assert_eq!(Shell::parse("bash"), Some(Shell::Bash));
        assert_eq!(Shell::parse("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("fish"), Some(Shell::Fish));
        assert_eq!(Shell::parse("tcsh"), None);
    }

    #[test]
    fn from_shell_var_strips_path() {
        assert_eq!(Shell::from_shell_var("/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(
            Shell::from_shell_var("/usr/local/bin/fish"),
            Some(Shell::Fish)
        );
        assert_eq!(Shell::from_shell_var("/bin/tcsh"), None);
    }

    #[test]
    fn fingerprints_distinguish_auto_installable_shells() {
        let zsh = String::from_utf8(generate_to_buf(Shell::Zsh)).unwrap();
        assert!(zsh.starts_with(Shell::Zsh.fingerprint().unwrap()));
        let bash = String::from_utf8(generate_to_buf(Shell::Bash)).unwrap();
        assert!(bash.contains(Shell::Bash.fingerprint().unwrap()));
        let fish = String::from_utf8(generate_to_buf(Shell::Fish)).unwrap();
        assert!(fish.contains(Shell::Fish.fingerprint().unwrap()));
        // Non-installable shells expose no fingerprint — install /
        // uninstall paths short-circuit on this.
        assert_eq!(Shell::PowerShell.fingerprint(), None);
        assert_eq!(Shell::Elvish.fingerprint(), None);
    }

    #[test]
    fn powershell_and_elvish_generate_nonempty_scripts() {
        // Even though they're not auto-installed, the explicit
        // `drip completions <shell>` command must still produce a
        // usable script.
        let ps = String::from_utf8(generate_to_buf(Shell::PowerShell)).unwrap();
        assert!(ps.len() > 200, "powershell script too small");
        assert!(ps.contains("drip"), "powershell script missing drip");
        let elv = String::from_utf8(generate_to_buf(Shell::Elvish)).unwrap();
        assert!(elv.len() > 100, "elvish script too small");
        assert!(elv.contains("drip"));
    }

    #[test]
    fn shell_var_only_matches_auto_installable_shells() {
        // Even if someone sets SHELL=/usr/bin/pwsh, we don't pretend
        // to know how to install for it via the unix-style flow.
        assert_eq!(Shell::from_shell_var("/usr/bin/pwsh"), None);
        assert_eq!(Shell::from_shell_var("/bin/zsh"), Some(Shell::Zsh));
    }
}
