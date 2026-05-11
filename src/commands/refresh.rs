use crate::core::session::{self, Session};
use anyhow::Result;

/// Drop the cached baseline for one file so the next read returns the
/// full content. Useful after an out-of-band change (manual edit in
/// another editor, `git pull`, …) when DRIP would otherwise hand the
/// agent a delta against a stale snapshot.
///
/// An OOB edit invalidates baselines across every session that has
/// read the file — so refresh drops them all, not just the caller's
/// derived session. Otherwise `drip refresh foo.py` typed in the
/// user's shell would silently miss the baseline living in the
/// agent's session and the next agent read would diff against a
/// stale snapshot.
pub fn run(file: &str) -> Result<String> {
    let session = Session::open()?;
    let resolved = session::resolve_path(file);
    let canonical = resolved
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned());

    let affected = session.delete_read_all_sessions(&canonical)?;

    Ok(match affected {
        0 => format!("No baseline tracked for {canonical} — nothing to clear.\n"),
        1 => format!("Cleared baseline for {canonical}\nNext read will return the full file.\n"),
        n => format!(
            "Cleared baseline for {canonical} in {n} sessions\nNext read in any session will return the full file.\n"
        ),
    })
}
