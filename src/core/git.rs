//! Pure-file-IO git context detection — no subprocess, no libgit2.
//!
//! Used by `derive_session()` to make session ids stable across crashes
//! (same cwd + same branch ⇒ same id) and isolated by branch (different
//! branch ⇒ different id, so a diff computed on one branch can't leak
//! into another).
//!
//! This intentionally implements only the *narrow* slice of git layout
//! we need: locating the `gitdir`, reading `HEAD`, parsing the symbolic
//! ref or detached commit hash. Anything fancier (packed refs,
//! submodules, gitlinks, sparse-checkout) is out of scope — if we can't
//! make sense of the layout we return `None` and the caller falls back
//! to the pid strategy. False negatives are fine; false positives (a
//! confidently-wrong branch name) would silently misroute reads, so
//! every parsing step is a best-effort that bails on the slightest
//! ambiguity.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Bound for any single file we read from a git layout. `HEAD` is at
/// most ~80 bytes (`ref: refs/heads/<name>\n` or a 41-char sha line);
/// a `.git` gitlink pointer is similarly tiny. Cap defends against a
/// hostile or corrupt repo planting a multi-GB blob to OOM the hook.
const GIT_FILE_CAP: u64 = 4 * 1024;

fn read_capped(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let mut buf = String::new();
    f.take(GIT_FILE_CAP).read_to_string(&mut buf).ok()?;
    Some(buf)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    /// Branch name (`main`, `feature/auth`, …) for an attached HEAD,
    /// or the first 8 chars of the commit sha when HEAD is detached.
    pub branch: String,
    /// Empty for the primary worktree, the full gitdir path for any
    /// secondary worktree (created via `git worktree add`). Two
    /// worktrees of the same repo on the same branch must hash to
    /// distinct session ids — including this in the key guarantees it.
    pub worktree_id: String,
}

/// Walk up from `cwd` looking for a `.git` entry, then resolve it to
/// the real `gitdir`. Returns `(gitdir, worktree_id)` where
/// `worktree_id` is empty for the primary worktree.
///
/// Returns `None` for any failure: not in a repo, unreadable `.git`,
/// missing target on a gitlink, malformed pointer.
fn find_gitdir(cwd: &Path) -> Option<(PathBuf, String)> {
    let mut here = cwd.to_path_buf();
    loop {
        let dot = here.join(".git");
        let meta = fs::symlink_metadata(&dot).ok();
        if let Some(meta) = meta {
            if meta.is_dir() {
                return Some((dot, String::new()));
            }
            if meta.is_file() {
                // gitlink: ".git" file containing "gitdir: <path>".
                let s = read_capped(&dot)?;
                let pointer = s.strip_prefix("gitdir:")?.trim();
                if pointer.is_empty() {
                    return None;
                }
                let mut p = PathBuf::from(pointer);
                if p.is_relative() {
                    // Pointer is relative to the .git file's directory.
                    p = here.join(p);
                }
                if !p.exists() {
                    return None;
                }
                let id = p.to_string_lossy().into_owned();
                return Some((p, id));
            }
        }
        if !here.pop() {
            return None;
        }
    }
}

/// Read the branch name (or short sha for detached HEAD) from `gitdir`.
fn read_branch(gitdir: &Path) -> Option<String> {
    let head = read_capped(&gitdir.join("HEAD"))?;
    let trimmed = head.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("ref: ") {
        let rest = rest.trim();
        if let Some(branch) = rest.strip_prefix("refs/heads/") {
            if branch.is_empty() {
                return None;
            }
            return Some(branch.to_string());
        }
        // Some other symbolic ref (refs/remotes/…) — bail.
        return None;
    }
    // Detached HEAD: 40-char hex (or 64 for SHA-256 repos).
    if trimmed.chars().all(|c| c.is_ascii_hexdigit()) && trimmed.len() >= 8 {
        return Some(trimmed[..8].to_string());
    }
    None
}

/// Best-effort detection. Always returns `Ok(None)` rather than `Err`
/// for any "not in a repo / can't read git layout" condition — the
/// caller falls back to the pid strategy.
pub fn detect(cwd: &Path) -> Option<GitContext> {
    let (gitdir, worktree_id) = find_gitdir(cwd)?;
    let branch = read_branch(&gitdir)?;
    Some(GitContext {
        branch,
        worktree_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn primary_repo_main_branch() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let ctx = detect(dir.path()).expect("detected");
        assert_eq!(ctx.branch, "main");
        assert_eq!(ctx.worktree_id, "");
    }

    #[test]
    fn nested_dir_walks_up_to_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/develop\n").unwrap();
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let ctx = detect(&nested).expect("detected");
        assert_eq!(ctx.branch, "develop");
    }

    #[test]
    fn slash_in_branch_name_kept_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git/HEAD"),
            "ref: refs/heads/feature/auth-rewrite\n",
        )
        .unwrap();
        assert_eq!(detect(dir.path()).unwrap().branch, "feature/auth-rewrite",);
    }

    #[test]
    fn detached_head_returns_short_sha() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let sha = "deadbeef0123456789abcdef0123456789abcdef";
        fs::write(dir.path().join(".git/HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(detect(dir.path()).unwrap().branch, "deadbeef");
    }

    #[test]
    fn missing_head_bails() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        // No HEAD file at all.
        assert!(detect(dir.path()).is_none());
    }

    #[test]
    fn empty_head_bails() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "").unwrap();
        assert!(detect(dir.path()).is_none());
    }

    #[test]
    fn garbage_head_bails_silently() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "garbage line\n").unwrap();
        assert!(detect(dir.path()).is_none());
    }

    #[test]
    fn no_repo_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_none());
    }

    #[test]
    fn worktree_resolves_to_secondary_gitdir() {
        let primary = tempfile::tempdir().unwrap();
        let primary_git = primary.path().join(".git");
        fs::create_dir_all(primary_git.join("worktrees/wt-x")).unwrap();
        fs::write(primary_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let wt_gitdir = primary_git.join("worktrees/wt-x");
        fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();

        let wt = tempfile::tempdir().unwrap();
        fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();

        let ctx = detect(wt.path()).expect("worktree detected");
        assert_eq!(ctx.branch, "feature/x");
        assert!(!ctx.worktree_id.is_empty(), "worktree_id must be set");
    }

    #[test]
    fn gitlink_pointing_to_missing_dir_bails() {
        let wt = tempfile::tempdir().unwrap();
        fs::write(wt.path().join(".git"), "gitdir: /nonexistent/path/.git\n").unwrap();
        assert!(detect(wt.path()).is_none());
    }
}
