/// Rough token estimator. Anthropic and OpenAI tokenizers both average
/// ~4 chars/token on code; we use that heuristic here.
///
/// We deliberately avoid pulling in `tiktoken-rs` or similar:
/// - Keeps the binary < 5MB and startup < 10ms.
/// - The estimator is only used for stats reporting, not billing.
pub fn estimate(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let bytes = text.len() as f64;
    (bytes / 4.0).ceil() as i64
}

pub fn percent_saved(full: i64, sent: i64) -> u32 {
    if full <= 0 {
        return 0;
    }
    let saved = (full - sent).max(0) as f64;
    ((saved / full as f64) * 100.0).round() as u32
}

/// USD per million input tokens used to convert `tokens_saved` into a
/// dollar figure. Defaults to Claude Sonnet 4.6 input pricing ($3 / Mtok)
/// — override with `DRIP_PRICE_PER_MTOK=0.8` (Haiku 4.5), `=15` (Opus 4.7),
/// `=2.5` (GPT-5 input), etc., to match your actual workload.
pub const DEFAULT_PRICE_PER_MTOK: f64 = 3.0;

/// Grams of CO₂e per 1,000 input tokens for typical cloud-GPU LLM
/// inference. Sourced from published estimates (~0.4 g/Ktok for a
/// medium-sized model on grid mix). Conservative round number; users
/// can override via `DRIP_CO2_G_PER_KTOK` if they want a different
/// figure.
pub const DEFAULT_CO2_G_PER_KTOK: f64 = 0.4;

pub fn price_per_mtok() -> f64 {
    std::env::var("DRIP_PRICE_PER_MTOK")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        .unwrap_or(DEFAULT_PRICE_PER_MTOK)
}

pub fn co2_g_per_ktok() -> f64 {
    std::env::var("DRIP_CO2_G_PER_KTOK")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        .unwrap_or(DEFAULT_CO2_G_PER_KTOK)
}

pub fn dollars_saved(tokens: i64) -> f64 {
    (tokens.max(0) as f64 / 1_000_000.0) * price_per_mtok()
}

pub fn co2_g_saved(tokens: i64) -> f64 {
    (tokens.max(0) as f64 / 1_000.0) * co2_g_per_ktok()
}
