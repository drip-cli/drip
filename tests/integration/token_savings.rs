use crate::common::Drip;
use std::fs;

#[test]
fn delta_read_reports_significant_savings() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.py");

    // 200 lines, change exactly one of them on second read.
    let mut v1 = String::new();
    for i in 0..200 {
        v1.push_str(&format!("line_{i:03} = compute_value({i})\n"));
    }
    let mut v2 = v1.clone();
    v2 = v2.replace(
        "line_100 = compute_value(100)",
        "line_100 = compute_value(101)",
    );
    fs::write(&f, &v1).unwrap();

    drip.read_stdout(&f);
    fs::write(&f, &v2).unwrap();
    let out = drip.read_stdout(&f);

    // Header form: "[DRIP: delta only | 87% token reduction (X/Y) | path]"
    let header = out.lines().next().unwrap_or("");
    assert!(header.contains("[DRIP: delta only"), "header: {header}");

    let pct = parse_percent(header).expect("percent in header");
    assert!(
        pct >= 80,
        "expected >=80% reduction for 1-line change in 200-line file, got {pct}% — header: {header}"
    );
}

#[test]
fn meter_reports_aggregate_stats() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.txt");
    fs::write(&f, "x\n".repeat(50)).unwrap();
    drip.read_stdout(&f);
    fs::write(&f, "x\n".repeat(50) + "y\n").unwrap();
    drip.read_stdout(&f);

    let stats = drip.meter();
    let lower = stats.to_lowercase();
    assert!(stats.contains("DRIP"), "stats: {stats}");
    assert!(lower.contains("files tracked"), "stats: {stats}");
    assert!(lower.contains("tokens saved"), "stats: {stats}");
}

fn parse_percent(header: &str) -> Option<u32> {
    // "[DRIP: delta only | 87% token reduction ..."
    let idx = header.find('%')?;
    let prefix = &header[..idx];
    let num: String = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    num.parse().ok()
}
