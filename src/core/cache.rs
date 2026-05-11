//! Hash-addressed file cache for large `reads.content` blobs.
//!
//! Above `DRIP_INLINE_MAX_BYTES` (default 32 KB), content is offloaded
//! to `<DRIP_DATA_DIR>/cache/<sha256>.bin`; the `reads` row stores only
//! the hash plus a `content_storage='file'` marker. Identical content
//! across files / sessions deduplicates by virtue of the hash key.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// `reads.content_storage` markers.
pub const STORAGE_INLINE: &str = "inline";
pub const STORAGE_FILE: &str = "file";

/// Defense in depth: reject anything that isn't a 64-char ASCII-hex
/// SHA-256. `Path::join` would let `"../../escape"` traverse out of
/// the cache dir, so every cache entry point validates.
fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `DRIP_INLINE_MAX_BYTES` default — typical source files stay
/// inline; outliers get hoisted to the file cache.
pub const DEFAULT_INLINE_MAX_BYTES: usize = 32 * 1024;

/// `-1` means "everything inline" (disable the file cache).
pub fn inline_max_bytes() -> usize {
    match std::env::var("DRIP_INLINE_MAX_BYTES") {
        Err(_) => DEFAULT_INLINE_MAX_BYTES,
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed == "-1" {
                return usize::MAX;
            }
            trimmed.parse::<usize>().unwrap_or(DEFAULT_INLINE_MAX_BYTES)
        }
    }
}

/// `<= threshold` stays inline (matches the user-facing wording).
pub fn pick_storage(content_len: usize) -> &'static str {
    if content_len <= inline_max_bytes() {
        STORAGE_INLINE
    } else {
        STORAGE_FILE
    }
}

pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache")
}

/// Full path to a single blob. Errors on invalid hashes — see
/// [`is_valid_hash`].
pub fn blob_path(data_dir: &Path, content_hash: &str) -> Result<PathBuf> {
    if !is_valid_hash(content_hash) {
        return Err(anyhow!(
            "refusing to compute cache path for non-hex hash '{content_hash}'"
        ));
    }
    Ok(cache_dir(data_dir).join(format!("{content_hash}.bin")))
}

/// Create the cache directory with `0o700` perms applied *atomically*
/// at creation time (Unix), so there is no window where a mode-022
/// umask leaks read access to other local users between `mkdir(2)`
/// and the follow-up `chmod`. On non-Unix this falls back to the
/// regular `create_dir_all`.
fn ensure_cache_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        // Re-apply 0o700 in case a manual mkdir loosened perms.
        harden_dir(dir);
        return Ok(());
    }
    create_dir_700(dir).with_context(|| format!("creating cache dir {dir:?}"))
}

#[cfg(unix)]
fn create_dir_700(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // `parent` may not exist either (DRIP_DATA_DIR override).
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_700(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Idempotent atomic write via tmp + rename. Refuses to short-circuit
/// on a symlink at the destination — a planted
/// `cache/<hash>.bin → /etc/passwd` would otherwise turn the next
/// `read_blob` into a confused deputy.
pub fn write_blob(data_dir: &Path, content_hash: &str, bytes: &[u8]) -> Result<()> {
    let final_path = blob_path(data_dir, content_hash)?;
    let dir = cache_dir(data_dir);
    ensure_cache_dir(&dir)?;

    // `symlink_metadata` does NOT traverse a symlink at the final
    // path component — lets us detect a planted link without
    // following it.
    match std::fs::symlink_metadata(&final_path) {
        Ok(m) if m.file_type().is_symlink() => {
            // `remove_file` unlinks the link itself, not the target.
            std::fs::remove_file(&final_path)
                .with_context(|| format!("removing symlink at cache path {final_path:?}"))?;
        }
        Ok(m) if m.is_file() => return Ok(()),
        Ok(_) => {
            return Err(anyhow!(
                "non-regular file at cache path {}: refusing to write",
                final_path.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("stat'ing cache path {final_path:?}"))
            )
        }
    }

    let tmp_path = final_path.with_extension("tmp");
    // `OpenOptions::mode` is honored only when O_CREAT actually
    // creates the inode, so unlink any orphaned tmp first to ensure
    // the new file is born at 0600 (DRIP blobs contain source content).
    let _ = std::fs::remove_file(&tmp_path);
    write_tmp_secure(&tmp_path, bytes)
        .with_context(|| format!("writing cache tmp {tmp_path:?}"))?;
    harden_file(&tmp_path);
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("renaming {tmp_path:?} -> {final_path:?}"))?;
    Ok(())
}

/// `O_CREAT | O_TRUNC | O_WRONLY` with mode 0600 on Unix. The mode
/// arg is honored only on inode creation; existing files rely on
/// `harden_file`'s post-write chmod.
fn write_tmp_secure(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// `Ok(None)` on missing file — callers treat that as a stale
/// baseline. **Refuses to follow symlinks** to prevent a planted
/// `cache/<hash>.bin → /etc/passwd` from leaking the link target as
/// the agent's baseline. Defense in depth on top of the dir's 0700.
pub fn read_blob(data_dir: &Path, content_hash: &str) -> Result<Option<String>> {
    let p = blob_path(data_dir, content_hash)?;
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_symlink() => Err(anyhow!(
            "refusing to read cache blob via symlink: {}",
            p.display()
        )),
        Ok(m) if !m.is_file() => Err(anyhow!("cache path is not a regular file: {}", p.display())),
        Ok(_) => {
            let bytes = std::fs::read(&p).with_context(|| format!("reading cache blob {p:?}"))?;
            // We only store UTF-8 (the differ rejects binary upstream).
            let s = String::from_utf8(bytes)
                .with_context(|| format!("non-UTF-8 in cache blob {p:?}"))?;
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("stat'ing cache blob {p:?}"))),
    }
}

/// Delete `cache/<hash>.bin` iff no `reads` row references it.
/// Conservative: validates hash shape, re-checks references after the
/// caller's DELETE, refuses to follow symlinks. Best-effort — IO
/// errors on individual blobs are skipped (orphans get reclaimed by
/// `drip cache gc`).
pub fn delete_blobs_if_unreferenced(
    conn: &Connection,
    data_dir: &Path,
    hashes: &[String],
) -> Result<usize> {
    if hashes.is_empty() {
        return Ok(0);
    }
    // A blob is still active if EITHER the per-session `reads`
    // table or the cross-session `file_registry` references it. The
    // session purge path only deletes from `reads`, so registry-only
    // references must keep the blob alive — otherwise the next-
    // session orientation hint loses its content.
    let mut check = conn.prepare_cached(
        "SELECT 1 FROM reads
         WHERE content_storage = 'file' AND content_hash = ?1
         UNION ALL
         SELECT 1 FROM file_registry
         WHERE content_storage = 'file' AND content_hash = ?1
         LIMIT 1",
    )?;
    let mut removed = 0usize;
    for h in hashes {
        if !is_valid_hash(h) {
            continue;
        }
        if check.exists(params![h]).unwrap_or(true) {
            // Either still referenced by a surviving row (dedup), or
            // the lookup failed — either way, leaving the blob is
            // the safe default.
            continue;
        }
        let path = match blob_path(data_dir, h) {
            Ok(p) => p,
            Err(_) => continue,
        };
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => continue,
            Ok(m) if m.is_file() && std::fs::remove_file(&path).is_ok() => {
                removed += 1;
            }
            _ => {}
        }
    }
    Ok(removed)
}

#[cfg(unix)]
fn harden_dir(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}

#[cfg(unix)]
fn harden_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_dir(_: &Path) {}
#[cfg(not(unix))]
fn harden_file(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_default_is_32k() {
        // Ensure the public constant matches the documented default.
        assert_eq!(DEFAULT_INLINE_MAX_BYTES, 32_768);
    }

    #[test]
    fn pick_storage_at_boundary() {
        // Use a tempenv would be ideal but `std::env::set_var` is
        // process-global; rely on default and reason about the math.
        let limit = DEFAULT_INLINE_MAX_BYTES;
        // Equality goes inline (per spec wording "< seuil ... < 32 KB").
        assert_eq!(pick_storage(0), STORAGE_INLINE);
        assert_eq!(pick_storage(limit), STORAGE_INLINE);
        assert_eq!(pick_storage(limit + 1), STORAGE_FILE);
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let body = "hello, drip\n".repeat(100);
        write_blob(dir.path(), hash, body.as_bytes()).unwrap();
        let got = read_blob(dir.path(), hash).unwrap().unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn missing_blob_returns_none_not_err() {
        let dir = tempfile::tempdir().unwrap();
        let got = read_blob(
            dir.path(),
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .unwrap();
        assert!(got.is_none(), "missing blob is Ok(None), not Err");
    }

    #[test]
    fn write_is_idempotent_on_existing_blob() {
        // Same hash = same content (by definition); a second write is
        // a no-op even if the bytes argument differs (we trust the
        // hash). Verifies the early-return path doesn't error.
        let dir = tempfile::tempdir().unwrap();
        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        write_blob(dir.path(), hash, b"first").unwrap();
        write_blob(dir.path(), hash, b"second-but-ignored").unwrap();
        let got = read_blob(dir.path(), hash).unwrap().unwrap();
        assert_eq!(got, "first", "second write must be a no-op");
    }

    // ── Security regressions ────────────────────────────────────────

    #[test]
    fn rejects_path_traversal_via_absolute_hash() {
        // `Path::join("/etc/passwd.bin")` would silently replace the
        // base path. Without is_valid_hash we'd happily try to write
        // outside the cache dir.
        let dir = tempfile::tempdir().unwrap();
        let err = blob_path(dir.path(), "/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("non-hex"));
        assert!(read_blob(dir.path(), "/etc/passwd").is_err());
        assert!(write_blob(dir.path(), "/etc/passwd", b"x").is_err());
    }

    #[test]
    fn rejects_path_traversal_via_dotdot_hash() {
        let dir = tempfile::tempdir().unwrap();
        assert!(blob_path(dir.path(), "../../escape").is_err());
        assert!(read_blob(dir.path(), "../../escape").is_err());
        assert!(write_blob(dir.path(), "../../escape", b"x").is_err());
    }

    #[test]
    fn rejects_short_or_non_hex_hash() {
        let dir = tempfile::tempdir().unwrap();
        // 63 chars (one too short).
        assert!(blob_path(
            dir.path(),
            "1111111111111111111111111111111111111111111111111111111111111"
        )
        .is_err());
        // 64 chars but with a non-hex letter (g).
        assert!(blob_path(
            dir.path(),
            "g1111111111111111111111111111111111111111111111111111111111111111"
                .get(0..64)
                .unwrap()
        )
        .is_err());
    }

    #[test]
    #[cfg(unix)]
    fn read_blob_refuses_to_follow_symlink() {
        // Plant a symlink at <hash>.bin pointing at a sensitive file.
        // read_blob must refuse rather than slurp the link target.
        let dir = tempfile::tempdir().unwrap();
        let hash = "2222222222222222222222222222222222222222222222222222222222222222";
        let cache = cache_dir(dir.path());
        std::fs::create_dir_all(&cache).unwrap();
        let secret = dir.path().join("would-be-leaked.txt");
        std::fs::write(&secret, b"super-secret").unwrap();
        let blob = cache.join(format!("{hash}.bin"));
        std::os::unix::fs::symlink(&secret, &blob).unwrap();

        let err = read_blob(dir.path(), hash).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("symlink"),
            "expected refusal mentioning 'symlink', got: {msg}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_blob_replaces_planted_symlink() {
        // Even if an attacker plants a symlink, the next write_blob
        // must remove it and write the real content. Otherwise
        // `final_path.exists()` would short-circuit and leave the
        // confused-deputy link in place forever.
        let dir = tempfile::tempdir().unwrap();
        let hash = "3333333333333333333333333333333333333333333333333333333333333333";
        let cache = cache_dir(dir.path());
        std::fs::create_dir_all(&cache).unwrap();
        let elsewhere = dir.path().join("attacker-target.txt");
        std::fs::write(&elsewhere, b"attacker payload").unwrap();
        let blob = cache.join(format!("{hash}.bin"));
        std::os::unix::fs::symlink(&elsewhere, &blob).unwrap();

        write_blob(dir.path(), hash, b"real content").unwrap();
        // The blob is now a regular file, the symlink target is
        // unmodified, and read_blob returns the real content.
        let m = std::fs::symlink_metadata(&blob).unwrap();
        assert!(!m.file_type().is_symlink());
        assert!(m.is_file());
        let elsewhere_content = std::fs::read(&elsewhere).unwrap();
        assert_eq!(
            elsewhere_content, b"attacker payload",
            "the symlink target must be untouched",
        );
        let got = read_blob(dir.path(), hash).unwrap().unwrap();
        assert_eq!(got, "real content");
    }

    #[test]
    #[cfg(unix)]
    fn cache_dir_is_created_with_strict_perms_atomically() {
        use std::os::unix::fs::PermissionsExt;
        // Pre-create the parent with a permissive umask so we can see
        // that the cache *child* dir still ends up 0o700 — proving the
        // DirBuilder mode applied at creation time, not just via a
        // follow-up chmod.
        let dir = tempfile::tempdir().unwrap();
        // Force a permissive umask in this thread; any leaked race
        // window between mkdir(2) and chmod would surface here.
        // (umask(2) returns the previous mask.)
        let prev = unsafe { libc::umask(0) };
        let hash = "4444444444444444444444444444444444444444444444444444444444444444";
        write_blob(dir.path(), hash, b"x").unwrap();
        unsafe { libc::umask(prev) };

        let mode = std::fs::metadata(cache_dir(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "cache dir leaked perms: {mode:o}");
    }
}
