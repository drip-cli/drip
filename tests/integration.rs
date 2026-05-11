//! Integration test entry point.
//!
//! Cargo only compiles `tests/*.rs` as integration crates. Submodules
//! under `tests/integration/` are pulled in here so they share a single
//! compilation unit and the `common` helper.

mod common;

#[path = "integration/diff_accuracy.rs"]
mod diff_accuracy;

#[path = "integration/session_persistence.rs"]
mod session_persistence;

#[path = "integration/token_savings.rs"]
mod token_savings;

#[path = "integration/edge_cases.rs"]
mod edge_cases;

#[path = "integration/mcp_server.rs"]
mod mcp_server;

#[path = "integration/codex_init.rs"]
mod codex_init;

#[path = "integration/claude_init.rs"]
mod claude_init;

#[path = "integration/doctor.rs"]
mod doctor;

#[path = "integration/completions.rs"]
mod completions;

#[path = "integration/cross_platform.rs"]
mod cross_platform;

#[path = "integration/agents_install.rs"]
mod agents_install;

#[path = "integration/registry.rs"]
mod registry;

#[path = "integration/post_edit_hook.rs"]
mod post_edit_hook;

#[path = "integration/read_offset_limit.rs"]
mod read_offset_limit;

#[path = "integration/session_keying.rs"]
mod session_keying;

#[path = "integration/storage.rs"]
mod storage;

#[path = "integration/concurrency.rs"]
mod concurrency;

#[path = "integration/diff_perf.rs"]
mod diff_perf;

#[path = "integration/dry_run.rs"]
mod dry_run;

#[path = "integration/meter_output.rs"]
mod meter_output;

#[path = "integration/regressions.rs"]
mod regressions;

#[path = "integration/dripignore.rs"]
mod dripignore;

#[path = "integration/watch.rs"]
mod watch;

#[path = "integration/replay.rs"]
mod replay;

#[path = "integration/compression.rs"]
mod compression;

#[path = "integration/source_map.rs"]
mod source_map;

#[path = "integration/session_start_hook.rs"]
mod session_start_hook;

#[path = "integration/pre_edit_hook.rs"]
mod pre_edit_hook;

#[path = "integration/reset_modes.rs"]
mod reset_modes;

#[path = "integration/gemini_compress_hook.rs"]
mod gemini_compress_hook;

#[path = "integration/accounting_audit.rs"]
mod accounting_audit;

#[path = "integration/agent_ux.rs"]
mod agent_ux;
