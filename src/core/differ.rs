use similar::TextDiff;

pub const LARGE_FILE_BYTES: usize = 100 * 1024;
pub const TRUNCATION_RATIO: f64 = 0.5;
pub const DEFAULT_CONTEXT: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Binary,
    TooLarge,
}

pub fn classify(bytes: &[u8]) -> FileKind {
    if bytes.len() > LARGE_FILE_BYTES {
        return FileKind::TooLarge;
    }
    if is_binary(bytes) {
        return FileKind::Binary;
    }
    FileKind::Text
}

/// A file is "binary" if it contains a NUL byte in the first 8KB or
/// fails UTF-8 validation. Mirrors git's heuristic closely enough.
pub fn is_binary(bytes: &[u8]) -> bool {
    let scan_len = bytes.len().min(8 * 1024);
    if bytes[..scan_len].contains(&0) {
        return true;
    }
    std::str::from_utf8(bytes).is_err()
}

/// Sharp drop in size — usually a destructive overwrite that reads better in full.
pub fn is_truncated(old_len: usize, new_len: usize) -> bool {
    if old_len == 0 {
        return false;
    }
    (new_len as f64) < (old_len as f64) * TRUNCATION_RATIO
}

pub fn unified_diff(
    file_label: &str,
    old: &str,
    new: &str,
    context_lines: usize,
) -> Option<String> {
    if old == new {
        return None;
    }
    let diff = TextDiff::from_lines(old, new);
    let header_old = format!("{file_label} (last read)");
    let header_new = format!("{file_label} (current)");
    let s = diff
        .unified_diff()
        .context_radius(context_lines)
        .header(&header_old, &header_new)
        .to_string();
    Some(s)
}

/// Structural shape of a unified diff — how many `@@` hunks, how many
/// added/removed lines, what fraction of the file changed, and the
/// span between the first and last hunk. Drives `is_too_complex`.
#[derive(Debug, Clone, Default)]
pub struct DiffComplexity {
    pub hunk_count: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub changed_pct: f32,
    pub max_hunk_distance: usize,
    /// `(line_in_new, hunk_header_line)` for each `@@` hunk header,
    /// in source order. Used by the renderer to build the
    /// `calculate_subtotal (ln 42), main (ln 156)` summary.
    pub hunk_starts: Vec<(usize, String)>,
}

/// Parse a unified diff string for shape only. Cheap — single pass,
/// no regex. The diff format is well-defined: lines starting with
/// `@@ -A,B +C,D @@` open a hunk; subsequent `+`/`-` lines (until
/// the next hunk or EOF) count as changes.
pub fn analyze_complexity(diff: &str, total_new_lines: usize) -> DiffComplexity {
    let mut c = DiffComplexity::default();
    let mut first_new_start: Option<usize> = None;
    let mut last_new_start: usize = 0;
    for line in diff.lines() {
        // Skip the file headers that aren't hunks.
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            // Format: `@@ -A,B +C,D @@ optional context tail`.
            // We want C — the start line in the new file.
            if let Some(new_start) = parse_hunk_new_start(rest) {
                if first_new_start.is_none() {
                    first_new_start = Some(new_start);
                }
                last_new_start = new_start;
                c.hunk_starts.push((new_start, line.to_string()));
                c.hunk_count += 1;
            }
            continue;
        }
        if line.starts_with('+') {
            c.added_lines += 1;
        } else if line.starts_with('-') {
            c.removed_lines += 1;
        }
    }
    let touched = c.added_lines + c.removed_lines;
    c.changed_pct = if total_new_lines == 0 {
        0.0
    } else {
        (touched as f32) / (total_new_lines as f32)
    };
    c.max_hunk_distance = match (first_new_start, c.hunk_count) {
        (Some(first), n) if n > 1 => last_new_start.saturating_sub(first),
        _ => 0,
    };
    c
}

fn parse_hunk_new_start(rest: &str) -> Option<usize> {
    // Find the `+C` token.
    let plus_idx = rest.find('+')?;
    let after = &rest[plus_idx + 1..];
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Default thresholds the user can override via env. These are the
/// numbers from the spec: 6 hunks, 40% changed, 200-line span.
const DEFAULT_MAX_HUNKS: usize = 6;
const DEFAULT_MAX_CHANGED_PCT: f32 = 0.40;
const DEFAULT_MAX_HUNK_DISTANCE: usize = 200;

/// Returns `true` when a unified diff is dispersed / large enough
/// that mentally applying it to the prior content is more error-
/// prone than just re-reading the file. The tracker uses this
/// signal to fall back on a `FullFallback { reason: DiffTooComplex }`
/// outcome.
pub fn is_too_complex(c: &DiffComplexity) -> bool {
    let max_hunks = std::env::var("DRIP_MAX_HUNKS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_HUNKS);
    let max_pct = std::env::var("DRIP_MAX_CHANGED_PCT")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .and_then(normalize_changed_pct)
        .unwrap_or(DEFAULT_MAX_CHANGED_PCT);
    c.hunk_count > max_hunks
        || c.changed_pct > max_pct
        || (c.hunk_count > 3 && c.max_hunk_distance > DEFAULT_MAX_HUNK_DISTANCE)
}

fn normalize_changed_pct(raw: f32) -> Option<f32> {
    if !raw.is_finite() || raw < 0.0 {
        None
    } else if raw > 1.0 {
        Some(raw / 100.0)
    } else {
        Some(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn binary_detected_via_nul() {
        assert!(is_binary(b"abc\0def"));
    }

    #[test]
    fn pure_ascii_not_binary() {
        assert!(!is_binary(b"hello world\nline two\n"));
    }

    #[test]
    fn invalid_utf8_is_binary() {
        assert!(is_binary(&[0xff, 0xfe, 0xfd, b'a']));
    }

    #[test]
    fn equal_inputs_yield_no_diff() {
        let s = "a\nb\nc\n";
        assert!(unified_diff("f", s, s, 3).is_none());
    }

    #[test]
    fn diff_marks_added_and_removed_lines() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nD\ne\nf\ng\n";
        let d = unified_diff("f.txt", old, new, 1).expect("diff");
        assert!(d.contains("-d"), "expected '-d' in diff:\n{d}");
        assert!(d.contains("+D"), "expected '+D' in diff:\n{d}");
        assert!(d.contains("--- f.txt (last read)"));
        assert!(d.contains("+++ f.txt (current)"));
    }

    #[test]
    fn truncation_threshold() {
        assert!(is_truncated(100, 40));
        assert!(!is_truncated(100, 60));
        assert!(!is_truncated(0, 0));
    }

    #[test]
    fn classify_routes_correctly() {
        assert_eq!(classify(b"hello\n"), FileKind::Text);
        assert_eq!(classify(b"a\0b"), FileKind::Binary);
        let big = vec![b'x'; LARGE_FILE_BYTES + 1];
        assert_eq!(classify(&big), FileKind::TooLarge);
    }

    #[test]
    fn changed_pct_env_accepts_fraction_or_percent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("DRIP_MAX_HUNKS");

        let c = DiffComplexity {
            hunk_count: 1,
            changed_pct: 0.30,
            max_hunk_distance: 0,
            ..Default::default()
        };

        std::env::set_var("DRIP_MAX_CHANGED_PCT", "0.40");
        assert!(!is_too_complex(&c), "0.40 must mean 40%, not 0.4%");

        std::env::set_var("DRIP_MAX_CHANGED_PCT", "40");
        assert!(
            !is_too_complex(&c),
            "40 must also mean 40% for CLI ergonomics"
        );

        std::env::set_var("DRIP_MAX_CHANGED_PCT", "20");
        assert!(is_too_complex(&c));

        std::env::remove_var("DRIP_MAX_CHANGED_PCT");
    }

    #[test]
    fn changed_pct_env_ignores_invalid_numbers() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("DRIP_MAX_HUNKS");

        let c = DiffComplexity {
            hunk_count: 1,
            changed_pct: 0.30,
            max_hunk_distance: 0,
            ..Default::default()
        };

        std::env::set_var("DRIP_MAX_CHANGED_PCT", "-1");
        assert!(
            !is_too_complex(&c),
            "negative env must fall back to default"
        );

        std::env::set_var("DRIP_MAX_CHANGED_PCT", "NaN");
        assert!(!is_too_complex(&c), "NaN env must fall back to default");

        std::env::remove_var("DRIP_MAX_CHANGED_PCT");
    }
}
