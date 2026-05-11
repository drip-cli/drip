//! Cross-platform orientation tests — minimal smoke checks that
//! confirm the binary, data dir, and a basic read flow work on
//! whichever OS the suite is running on. Real coverage comes from
//! the rest of the integration suite; this file is a quick sanity
//! gate for "did we just regress an OS we don't dev on."

use crate::common::Drip;
use std::fs;

#[cfg(unix)]
#[test]
fn binary_has_no_exe_extension_on_unix() {
    let drip = Drip::new();
    assert!(
        !drip.bin.ends_with(".exe"),
        "Unix binary should not have .exe suffix: {}",
        drip.bin
    );
}

#[cfg(windows)]
#[test]
fn binary_has_exe_extension_on_windows() {
    let drip = Drip::new();
    assert!(
        drip.bin.ends_with(".exe"),
        "Windows binary should be drip.exe, got: {}",
        drip.bin
    );
}

#[test]
fn read_roundtrip_works_on_this_platform() {
    // The classic two-read flow: full bytes the first time, diff /
    // unchanged the second. This is the smallest end-to-end check
    // that DB writes, file IO, and the CLI plumbing all work
    // identically on whichever OS we're on.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    fs::write(&f, "first\n").unwrap();
    let first = drip.read_stdout(&f);
    assert!(
        first.contains("first"),
        "first read should return content: {first}"
    );

    let second = drip.read_stdout(&f);
    assert!(
        second.contains("unchanged") || second.contains("first"),
        "second read of unchanged file should be unchanged or full: {second}"
    );
}

#[test]
fn data_dir_under_drip_data_dir_override() {
    // The DRIP_DATA_DIR override path is the cross-platform escape
    // hatch — every other resolution is OS-specific (Library/...,
    // ~/.local/share/..., AppData\Roaming\...). If the override
    // works, the OS-specific defaults inherit the same write logic.
    let drip = Drip::new();
    let f = drip.data_dir.path().join("drip-test.txt");
    fs::write(&f, "x\n").unwrap();
    drip.read_stdout(&f);
    assert!(
        drip.data_dir.path().join("sessions.db").exists(),
        "sessions.db should be created under DRIP_DATA_DIR"
    );
}
