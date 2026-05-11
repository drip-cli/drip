use crate::common::Drip;
use std::fs;
use std::time::Instant;

/// Generate `n_lines` of plausible code-shaped text so the diff engine
/// has realistic input.
fn synthetic(n_lines: usize) -> String {
    let mut s = String::with_capacity(n_lines * 40);
    for i in 0..n_lines {
        s.push_str(&format!(
            "fn handler_{i:04}(req: Request) -> Response {{ /* {i} */ inner({i}) }}\n"
        ));
    }
    s
}

fn time_read(drip: &Drip, file: &std::path::Path) -> std::time::Duration {
    let t0 = Instant::now();
    let o = drip
        .cmd()
        .arg("read")
        .arg(file)
        .output()
        .expect("drip read");
    assert!(o.status.success());
    t0.elapsed()
}

/// In debug builds, "5ms overhead" is unrealistic — debug rusqlite +
/// debug similar are 5–15× slower than release. We assert a generous
/// debug bound here; the strict 5ms target is enforced manually via
/// `scripts/bench.sh` against the release binary.
const DEBUG_BUDGET_MS: u128 = 250;

#[test]
fn diff_under_budget_50kb() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("c50.rs");

    // ~50KB
    let v1 = synthetic(900);
    fs::write(&f, &v1).unwrap();
    let _ = time_read(&drip, &f); // first read primes baseline

    // change one line in the middle
    let v2 = v1.replace("handler_0450", "handler_X450");
    fs::write(&f, &v2).unwrap();

    let took = time_read(&drip, &f);
    assert!(
        took.as_millis() < DEBUG_BUDGET_MS,
        "50KB diff took {}ms (debug budget {}ms) — release target is <5ms",
        took.as_millis(),
        DEBUG_BUDGET_MS
    );
}

#[test]
fn diff_under_budget_99kb() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("c99.rs");

    // ~99KB — just under the LARGE_FILE_BYTES threshold.
    let v1 = synthetic(1750);
    fs::write(&f, &v1).unwrap();
    let _ = time_read(&drip, &f);

    let v2 = v1.replace("handler_0875", "handler_X875");
    fs::write(&f, &v2).unwrap();

    let took = time_read(&drip, &f);
    assert!(
        took.as_millis() < DEBUG_BUDGET_MS,
        "99KB diff took {}ms (debug budget {}ms) — release target is <5ms",
        took.as_millis(),
        DEBUG_BUDGET_MS
    );
}

#[test]
fn unchanged_path_is_fast() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("u.rs");
    fs::write(&f, synthetic(800)).unwrap();
    let _ = time_read(&drip, &f);

    let took = time_read(&drip, &f);
    assert!(
        took.as_millis() < DEBUG_BUDGET_MS,
        "unchanged read took {}ms (debug budget {}ms)",
        took.as_millis(),
        DEBUG_BUDGET_MS
    );
}
