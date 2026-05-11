use crate::common::Drip;
use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_read(drip: &Drip, file: &std::path::Path) -> Child {
    Command::new(&drip.bin)
        .arg("read")
        .arg(file)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn drip read")
}

#[test]
fn three_concurrent_agents_no_deadlock() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    // Each "agent" hammers the same set of files in parallel. WAL mode +
    // 500ms busy_timeout should serialize writers cleanly with no deadlock.
    let files: Vec<_> = (0..5)
        .map(|i| {
            let p = dir.path().join(format!("shared_{i}.txt"));
            fs::write(&p, format!("file {i} v1\n").repeat(40)).unwrap();
            p
        })
        .collect();

    let started = Instant::now();
    let mut children = Vec::new();
    for _agent in 0..3 {
        for f in &files {
            children.push(spawn_read(&drip, f));
        }
    }

    // Hard ceiling: any reasonable run finishes in < 30s. If we hit it,
    // we're deadlocked or terribly slow — kill the run rather than hang CI.
    let deadline = started + Duration::from_secs(30);
    for mut c in children {
        loop {
            if let Some(status) = c.try_wait().expect("try_wait") {
                assert!(
                    status.success(),
                    "concurrent drip read failed with {status:?}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "deadlock or stall: child still running after 30s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn parallel_writers_to_same_file_serialize_cleanly() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hot.txt");
    fs::write(&f, "v0\n").unwrap();

    // Spawn 6 readers of the same file at once. The first to land in the
    // DB wins the "first read"; the others either also see "first read"
    // (if they raced past the SELECT) or see "unchanged". Either is fine
    // — the key invariant is: they all exit cleanly.
    let mut children: Vec<Child> = (0..6).map(|_| spawn_read(&drip, &f)).collect();
    for c in children.iter_mut() {
        let status = c.wait().expect("wait");
        assert!(status.success(), "concurrent reader failed: {status:?}");
    }
}
