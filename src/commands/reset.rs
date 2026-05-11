use crate::core::session::{self, Session};
use anyhow::{anyhow, Result};
use std::io::{BufRead, Write};

pub struct ResetOpts {
    pub all: bool,
    pub stats: bool,
    pub force: bool,
}

pub fn run(opts: ResetOpts) -> Result<String> {
    if opts.all && opts.stats {
        return Err(anyhow!(
            "--all and --stats are mutually exclusive — `--all` already wipes the lifetime counters \
             along with everything else"
        ));
    }
    if opts.all {
        return run_reset_all(opts.force);
    }
    if opts.stats {
        return run_reset_stats();
    }
    run_reset_session()
}

/// Default — clears the current session only. No-flag fallback.
fn run_reset_session() -> Result<String> {
    let session = Session::open()?;
    let id = session.id.clone();
    session.reset()?;
    Ok(format!("Session {id} cleared.\n"))
}

/// `--stats` — zeros the cumulative-since-install counters without
/// touching active sessions or per-file baselines. Right call when a
/// bench run pollutes the lifetime numbers and you want to keep
/// working on whatever you were working on.
fn run_reset_stats() -> Result<String> {
    let session = Session::open()?;
    let report = session.reset_lifetime_stats()?;
    Ok(format!(
        "Lifetime counters cleared.\n  - {} lifetime_stats row\n  \
         - {} per-file rows\n  - {} daily rows\n  - {} edited-file rows\n\
         Active sessions and per-file baselines untouched.\n",
        report.stats_rows, report.per_file_rows, report.daily_rows, report.edited_rows,
    ))
}

/// `--all` — nukes every row in every table plus every blob on disk.
/// Irreversible. We require an explicit `yes` from stdin (or `--force`
/// for scripts) because this throws away the user's lifetime savings
/// figures and their entire cross-session file registry.
fn run_reset_all(force: bool) -> Result<String> {
    if !force && !confirm_reset_all()? {
        return Ok("Aborted — no data was deleted.\n".into());
    }
    let report = session::reset_all_data()?;
    Ok(format!(
        "All DRIP data cleared.\n  - {} sessions\n  - {} reads\n  \
         - {} registry entries\n  \
         - {} cache blobs ({} bytes)\n  - {} lifetime stat rows\n\
         Lifetime token-savings counters reset to zero.\n",
        report.sessions,
        report.reads,
        report.registry,
        report.cache_blobs,
        report.cache_bytes,
        report.lifetime_rows,
    ))
}

/// Prompt to stderr (so stdout stays clean for scripted captures of
/// the result message) and read `yes` from stdin. We deliberately
/// don't accept `y` / `Y` — `--all` is destructive and muscle-memory
/// typos shouldn't be enough.
fn confirm_reset_all() -> Result<bool> {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    write!(
        handle,
        "This will permanently delete all DRIP data including \
         lifetime token savings. Type 'yes' to confirm: "
    )?;
    handle.flush()?;
    drop(handle);

    let mut buf = String::new();
    std::io::stdin().lock().read_line(&mut buf)?;
    Ok(buf.trim().eq_ignore_ascii_case("yes"))
}
