use crate::hooks::{
    claude, claude_glob, claude_grep, claude_post_edit, claude_pre_edit, claude_session_start,
    gemini, gemini_compress,
};
use anyhow::Result;
use std::io::{self, Read};

#[derive(Debug, Clone, Copy)]
pub enum HookAgent {
    Claude,
    ClaudeGlob,
    ClaudeGrep,
    ClaudePostEdit,
    ClaudePreEdit,
    ClaudeSessionStart,
    Gemini,
    /// Gemini CLI before-compress hook (advisory). Resets per-session
    /// baselines + bumps the v9 compaction ledger so the next read
    /// after Gemini's context compression is a genuine FullFirst the
    /// agent's read tracker registers.
    GeminiCompress,
}

pub fn run(agent: HookAgent) -> Result<String> {
    let agent_tag: Option<&str> = match agent {
        HookAgent::Claude
        | HookAgent::ClaudeGlob
        | HookAgent::ClaudeGrep
        | HookAgent::ClaudePostEdit
        | HookAgent::ClaudePreEdit
        | HookAgent::ClaudeSessionStart => Some("claude"),
        HookAgent::Gemini | HookAgent::GeminiCompress => Some("gemini"),
    };
    if let Some(t) = agent_tag {
        std::env::set_var("DRIP_AGENT", t);
    }

    // Bound the hook payload to 4 MiB. Agent payloads in the wild are
    // tiny JSON objects — kilobytes at most. An unbounded read here is
    // a trivial DoS (a hostile or buggy agent could pipe `/dev/zero`
    // or a multi-GB blob into the hook and pin the process on memory).
    // We read 4 MiB + 1 byte: if the +1 byte arrives we know the cap
    // was exceeded and bail rather than silently truncating, which
    // would hand the downstream JSON parser malformed input.
    const HOOK_PAYLOAD_CAP: u64 = 4 * 1024 * 1024;
    let mut buf = String::new();
    let read = io::stdin()
        .take(HOOK_PAYLOAD_CAP + 1)
        .read_to_string(&mut buf)?;
    if read as u64 > HOOK_PAYLOAD_CAP {
        anyhow::bail!(
            "drip: hook payload exceeds {} byte cap — refusing to parse",
            HOOK_PAYLOAD_CAP
        );
    }
    match agent {
        HookAgent::Claude => claude::handle(&buf),
        HookAgent::ClaudeGlob => claude_glob::handle(&buf),
        HookAgent::ClaudeGrep => claude_grep::handle(&buf),
        HookAgent::ClaudePostEdit => claude_post_edit::handle(&buf),
        HookAgent::ClaudePreEdit => claude_pre_edit::handle(&buf),
        HookAgent::ClaudeSessionStart => claude_session_start::handle(&buf),
        HookAgent::Gemini => gemini::handle(&buf),
        HookAgent::GeminiCompress => gemini_compress::handle(&buf),
    }
}
