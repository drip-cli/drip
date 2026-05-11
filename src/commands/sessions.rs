use crate::core::session::{unix_now, Session};
use anyhow::Result;
use rusqlite::params;

pub fn run() -> Result<String> {
    // Read-only: don't create a `sessions` row just to list sessions.
    let session = Session::open_readonly()?;
    let mut stmt = session.conn.prepare(
        "SELECT s.session_id, s.started_at, s.last_active, s.cwd,
                s.strategy, s.context, s.agent,
                COALESCE((SELECT COUNT(*) FROM reads r WHERE r.session_id = s.session_id), 0),
                COALESCE((SELECT SUM(tokens_full - tokens_sent) FROM reads r WHERE r.session_id = s.session_id), 0)
         FROM sessions s
         ORDER BY s.last_active DESC",
    )?;
    let rows = stmt
        .query_map(params![], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let now = unix_now();

    // Column width = the widest session id we'll print (plus 1 for
    // the `*` marker prefix). Prevents the table from skewing when
    // a Claude UUID (36 chars) sits next to a 16-hex git id.
    let id_col = rows
        .iter()
        .map(|r| r.0.chars().count())
        .max()
        .unwrap_or(16)
        .max(7); // floor at "SESSION" header width

    let mut out = String::new();
    out.push_str(&format!(
        "{:<idw$} {:<11} {:<8} {:<20} {:>6} {:>6} {:>10}  {}\n",
        "SESSION",
        "AGENT",
        "STRATEGY",
        "CONTEXT",
        "AGE",
        "FILES",
        "SAVED",
        "CWD",
        idw = id_col + 1,
    ));
    for (id, started, _last, cwd, strategy, context, agent, files, saved) in rows {
        let marker = if id == session.id { "*" } else { " " };
        let strategy = strategy.unwrap_or_else(|| "-".to_string());
        let context = context.unwrap_or_else(|| "-".to_string());
        // Truncate the context to keep the row in ~100 cols when
        // branch names get long (`feature/some-elaborate-name`).
        let context_disp = truncate(&context, 20);
        let agent_disp = render_agent(agent.as_deref(), &strategy, &id);
        out.push_str(&format!(
            "{marker}{:<idw$} {:<11} {:<8} {:<20} {:>6} {:>6} {:>10}  {}\n",
            id,
            agent_disp,
            strategy,
            context_disp,
            human_short(now - started),
            files,
            format_thousands(saved.max(0)),
            cwd.unwrap_or_default(),
            idw = id_col,
        ));
    }
    Ok(out)
}

/// Pick the best label for *who* created this session.
///
/// Resolve the agent label for a session row. Priority:
/// 1. `sessions.agent` (populated from `$DRIP_AGENT`).
/// 2. Heuristic on strategy + id shape.
fn render_agent(persisted: Option<&str>, strategy: &str, id: &str) -> String {
    if let Some(tag) = persisted {
        return match tag {
            "claude" => "Claude Code".into(),
            "codex" => "Codex".into(),
            "gemini" => "Gemini".into(),
            // Unknown tags: surface the raw value rather than hiding
            // it behind "shell".
            other => other.to_string(),
        };
    }
    match strategy {
        "env" if is_uuid_v4(id) => "Claude Code".into(),
        "env" => "custom".into(),
        _ => "shell".into(),
    }
}

/// True for a canonical hyphenated 36-char UUID v4. The shape alone is
/// distinctive vs the 16-hex git ids and short pid/cwd hashes.
fn is_uuid_v4(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.chars().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => c == '-',
        _ => c.is_ascii_hexdigit(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(2);
    let head: String = s.chars().take(take).collect();
    format!("{head}..")
}

fn format_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    if n < 0 {
        out.push('-');
    }
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn human_short(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
