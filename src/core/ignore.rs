//! `.dripignore` matcher — gitignore-flavored patterns applied to file
//! reads and Grep/Glob results.
//!
//! Lookup order: `$DRIP_IGNORE_FILE` → `./.dripignore` → `~/.dripignore`
//! → built-in defaults.
//!
//! Patterns: one glob per line, `#` for comments, leading `!` negates,
//! `**` matches any number of components. Not 100% gitignore-compatible
//! — anchored vs. unanchored corner cases aren't covered.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

/// Default ignore set — kept tight on purpose: false positives break
/// workflows, false negatives just mean a missed save.
const DEFAULTS: &[&str] = &[
    // VCS and editor noise
    ".git/**",
    ".hg/**",
    ".svn/**",
    ".DS_Store",
    "**/.DS_Store",
    // Dependency directories
    "node_modules/**",
    "**/node_modules/**",
    "vendor/**",
    ".venv/**",
    "venv/**",
    "__pycache__/**",
    "**/__pycache__/**",
    // Build outputs
    "target/**",
    "dist/**",
    "build/**",
    ".next/**",
    ".turbo/**",
    ".svelte-kit/**",
    "out/**",
    // Lock files (huge, agent rarely needs full content)
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Cargo.lock",
    "Gemfile.lock",
    "poetry.lock",
    "uv.lock",
    "composer.lock",
    // Common binary asset extensions
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.webp",
    "*.ico",
    "*.bmp",
    "*.pdf",
    "*.zip",
    "*.tar",
    "*.gz",
    "*.tgz",
    "*.bz2",
    "*.7z",
    "*.so",
    "*.dylib",
    "*.dll",
    "*.a",
    "*.o",
    "*.exe",
    "*.woff",
    "*.woff2",
    "*.ttf",
    "*.eot",
    "*.otf",
    "*.mp4",
    "*.mov",
    "*.webm",
    "*.mp3",
    "*.wav",
    // Secrets & credentials. `file_registry` persists across sessions
    // until `drip registry gc`, so a single brush with `.env` would
    // bake the content into a long-lived DB row that backup tools
    // could carry off-host.
    ".env",
    ".env.*",
    "**/.env",
    "**/.env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.crt",
    "*.cer",
    "*.jks",
    "*.keystore",
    "id_rsa",
    "id_rsa.pub",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_ed25519.pub",
    "**/id_rsa",
    "**/id_rsa.pub",
    "**/id_dsa",
    "**/id_ecdsa",
    "**/id_ed25519",
    "**/id_ed25519.pub",
    "**/.ssh/**",
    "**/.aws/credentials",
    "**/.aws/config",
    "**/.gcp/credentials.json",
    ".netrc",
    "**/.netrc",
    ".npmrc",
    "**/.npmrc",
    ".pypirc",
    "**/.pypirc",
    "kubeconfig",
    "*.kubeconfig",
    "**/kubeconfig",
];

#[derive(Debug)]
pub struct Matcher {
    /// Patterns that mark a path as ignored.
    deny: GlobSet,
    /// Patterns that re-include an otherwise-ignored path (`!foo`).
    allow: GlobSet,
}

impl Matcher {
    /// Load + combine from all four lookup locations. Missing files
    /// contribute nothing; malformed lines log via `eprintln!`.
    pub fn load() -> Self {
        Self::load_with_root(None)
    }

    /// Load with an explicit project root — used by `drip watch` when
    /// cwd is elsewhere.
    pub fn load_with_root(root: Option<&Path>) -> Self {
        let mut deny = GlobSetBuilder::new();
        let mut allow = GlobSetBuilder::new();

        // Defaults first — overridable by user files via `!pattern`.
        for raw in DEFAULTS {
            push_pattern(&mut deny, &mut allow, raw);
        }

        for path in lookup_files(root) {
            if let Some(text) = read_dripignore_capped(&path) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    push_pattern(&mut deny, &mut allow, line);
                }
            }
        }

        let deny = deny.build().unwrap_or_else(|_| empty_set());
        let allow = allow.build().unwrap_or_else(|_| empty_set());
        Matcher { deny, allow }
    }

    /// Loaded directly from a string — for tests and `--ignore-file=-`.
    #[allow(dead_code)]
    pub fn from_str(text: &str) -> Result<Self> {
        let mut deny = GlobSetBuilder::new();
        let mut allow = GlobSetBuilder::new();
        for raw in DEFAULTS {
            push_pattern(&mut deny, &mut allow, raw);
        }
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            push_pattern(&mut deny, &mut allow, line);
        }
        Ok(Matcher {
            deny: deny.build().context("building deny globset")?,
            allow: allow.build().context("building allow globset")?,
        })
    }

    /// Match against the full path, the file name alone, and a
    /// `./`-stripped variant — so `package-lock.json` matches
    /// regardless of how the agent spelled the path.
    pub fn is_ignored(&self, path: &Path) -> bool {
        let denied = self.matches_path(path, &self.deny);
        if !denied {
            return false;
        }
        !self.matches_path(path, &self.allow)
    }

    fn matches_path(&self, path: &Path, set: &GlobSet) -> bool {
        if set.is_match(path) {
            return true;
        }
        if let Some(name) = path.file_name() {
            if set.is_match(Path::new(name)) {
                return true;
            }
        }
        if let Some(s) = path.to_str() {
            if let Some(stripped) = s.strip_prefix("./") {
                if set.is_match(Path::new(stripped)) {
                    return true;
                }
            }
        }
        false
    }
}

fn push_pattern(deny: &mut GlobSetBuilder, allow: &mut GlobSetBuilder, raw: &str) {
    let (negated, body) = match raw.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    // Expand `dir/` → `dir/**`; `globset` doesn't accept a bare
    // trailing slash, and the descendant form is what matters in
    // practice (DRIP only sees file paths).
    let body = canonicalize_dir_pattern(body);
    let g = match Glob::new(&body) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("drip: ignoring malformed .dripignore pattern {raw:?}: {e}");
            return;
        }
    };
    if negated {
        allow.add(g);
    } else {
        deny.add(g);
    }
}

/// Apply the gitignore "trailing slash = directory + descendants" rule.
/// Returns the input unchanged when the pattern doesn't end with `/`,
/// or already targets a recursive descendant (`/**`, `/**/...`).
fn canonicalize_dir_pattern(body: &str) -> String {
    if !body.ends_with('/') {
        return body.to_string();
    }
    // Already-explicit recursive forms — leave alone. We strip the
    let trimmed = body.trim_end_matches('/');
    if trimmed.ends_with("/**") || trimmed == "**" {
        return trimmed.to_string();
    }
    // Single `**` covers both immediate children and arbitrary depth.
    format!("{trimmed}/**")
}

fn lookup_files(root: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("DRIP_IGNORE_FILE") {
        out.push(std::path::PathBuf::from(p));
    }
    // Project root before cwd — the watcher's watched path beats its
    // own cwd.
    if let Some(r) = root {
        out.push(r.join(".dripignore"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(".dripignore");
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".dripignore"));
    }
    out
}

fn empty_set() -> GlobSet {
    GlobSetBuilder::new()
        .build()
        .expect("empty globset always builds")
}

/// Hard byte cap on `.dripignore` reads — defends against a multi-GB
/// file from a redirect typo. 256 KiB is generous (the linux kernel's
/// `.gitignore` is ~2 KB).
const DRIPIGNORE_CAP: u64 = 256 * 1024;

fn read_dripignore_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    f.take(DRIPIGNORE_CAP).read_to_string(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_node_modules() {
        let m = Matcher::from_str("").unwrap();
        assert!(m.is_ignored(Path::new("node_modules/foo/bar.js")));
        assert!(m.is_ignored(Path::new("project/node_modules/x.js")));
    }

    #[test]
    fn defaults_match_lock_files() {
        let m = Matcher::from_str("").unwrap();
        assert!(m.is_ignored(Path::new("package-lock.json")));
        assert!(m.is_ignored(Path::new("/abs/path/Cargo.lock")));
        assert!(m.is_ignored(Path::new("./yarn.lock")));
    }

    #[test]
    fn user_pattern_adds_to_defaults() {
        let m = Matcher::from_str("secrets/*.txt\n").unwrap();
        assert!(m.is_ignored(Path::new("secrets/foo.txt")));
        assert!(!m.is_ignored(Path::new("src/main.rs")));
    }

    #[test]
    fn negation_re_includes() {
        let m = Matcher::from_str("!Cargo.lock\n").unwrap();
        assert!(!m.is_ignored(Path::new("Cargo.lock")));
        // Other defaults still apply.
        assert!(m.is_ignored(Path::new("yarn.lock")));
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let m = Matcher::from_str("# comment\n\n  \nsecrets/*\n").unwrap();
        assert!(m.is_ignored(Path::new("secrets/x")));
    }

    #[test]
    fn malformed_pattern_does_not_panic() {
        let _ = Matcher::from_str("[unclosed\n").unwrap();
    }

    // ── gitignore trailing-slash semantics ──────────────────────────

    #[test]
    fn trailing_slash_matches_immediate_children() {
        let m = Matcher::from_str("playground/\n").unwrap();
        assert!(
            m.is_ignored(Path::new("playground/foo.txt")),
            "`playground/` must ignore immediate file children",
        );
    }

    #[test]
    fn trailing_slash_matches_arbitrary_depth() {
        let m = Matcher::from_str("playground/\n").unwrap();
        assert!(
            m.is_ignored(Path::new("playground/a/b.txt")),
            "`playground/` must ignore deeply-nested descendants",
        );
        assert!(
            m.is_ignored(Path::new("playground/a/b/c/d.rs")),
            "`playground/` must recurse arbitrarily",
        );
    }

    #[test]
    fn trailing_slash_does_not_leak_to_siblings() {
        let m = Matcher::from_str("playground/\n").unwrap();
        // `playgroundlol/foo` shares a prefix but not a path component
        // boundary — must NOT be ignored.
        assert!(!m.is_ignored(Path::new("playgroundlol/foo.txt")));
        assert!(!m.is_ignored(Path::new("not-playground/foo.txt")));
    }

    #[test]
    fn explicit_double_star_still_works_unchanged() {
        // The expansion is idempotent: `playground/**` written by the
        // user must keep working exactly as before, not become
        // `playground/**/**`.
        let m = Matcher::from_str("playground/**\n").unwrap();
        assert!(m.is_ignored(Path::new("playground/foo.txt")));
        assert!(m.is_ignored(Path::new("playground/a/b.txt")));
    }

    #[test]
    fn trailing_slash_negation_re_includes_descendants() {
        // `playground/` ignores everything inside; `!playground/keep.js`
        // re-includes one specific descendant. Both halves of the rule
        // must keep working post-expansion.
        let m = Matcher::from_str("playground/\n!playground/keep.js\n").unwrap();
        assert!(m.is_ignored(Path::new("playground/foo.txt")));
        assert!(!m.is_ignored(Path::new("playground/keep.js")));
    }

    #[test]
    fn canonicalize_dir_pattern_helper() {
        assert_eq!(canonicalize_dir_pattern("playground/"), "playground/**");
        assert_eq!(canonicalize_dir_pattern("a/b/"), "a/b/**");
        // No trailing slash → unchanged.
        assert_eq!(canonicalize_dir_pattern("playground"), "playground");
        // Already recursive → stripped trailing slash but no double
        // star duplication.
        assert_eq!(canonicalize_dir_pattern("playground/**"), "playground/**");
        assert_eq!(canonicalize_dir_pattern("playground/**/"), "playground/**");
    }
}
