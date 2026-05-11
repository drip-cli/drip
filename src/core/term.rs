//! Tiny terminal-color helper. No external dependency.
//!
//! - Honors `NO_COLOR=1` (https://no-color.org) and `--no-color`.
//! - Disables when stdout is not a TTY.
//! - Stays inert in unit tests (no color in non-tty pipes).

use std::io::IsTerminal;
use std::sync::OnceLock;

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

static FORCE: OnceLock<Option<bool>> = OnceLock::new();

#[allow(dead_code)]
pub fn force(enabled: bool) {
    let _ = FORCE.set(Some(enabled));
}

pub fn enabled() -> bool {
    if let Some(Some(forced)) = FORCE.get() {
        return *forced;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // Honor the de-facto npm / node convention `FORCE_COLOR=1` (and its
    // BSD-ish cousin `CLICOLOR_FORCE`) so users can pipe through `cat` /
    // `less -R` and still see colors.
    if matches!(
        std::env::var("FORCE_COLOR").as_deref(),
        Ok("1") | Ok("2") | Ok("3") | Ok("true")
    ) {
        return true;
    }
    if std::env::var("CLICOLOR_FORCE").as_deref() == Ok("1") {
        return true;
    }
    std::io::stdout().is_terminal()
}

fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

pub fn red(s: &str) -> String {
    paint(RED, s)
}

pub fn yellow(s: &str) -> String {
    paint(YELLOW, s)
}

pub fn green(s: &str) -> String {
    paint(GREEN, s)
}

pub fn dim(s: &str) -> String {
    paint(DIM, s)
}

pub fn bold(s: &str) -> String {
    paint(BOLD, s)
}

/// Color a savings percentage:
///   ≥70  green   (great)
///   30–69 yellow (decent)
///    <30 red    (low)
pub fn color_pct(pct: u32) -> String {
    let txt = format!("{pct}%");
    match pct {
        70..=u32::MAX => green(&txt),
        30..=69 => yellow(&txt),
        _ => red(&txt),
    }
}
