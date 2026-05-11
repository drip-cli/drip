use crate::commands::read;
use crate::core::session;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GeminiPayload {
    file_path: String,
}

pub fn handle(stdin_payload: &str) -> Result<String> {
    let p: GeminiPayload =
        serde_json::from_str(stdin_payload).context("expected { \"file_path\": ... }")?;

    // Honour the universal kill switch — if DRIP_DISABLE is set, bypass
    // delta interception entirely and return the file's raw content,
    // mirroring the documented "bypasses interception entirely without
    // uninstalling the hooks" contract that the Claude / Bash / Glob /
    // Grep / SessionStart hooks already implement.
    if std::env::var_os("DRIP_DISABLE").is_some() {
        let resolved = session::resolve_path(&p.file_path);
        return std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()));
    }
    read::run(&p.file_path)
}
