//! Per-model pricing + context-window table and cost / ctx% calculation
//! (FR-3/AC-3, FR-3/AC-4).
//!
//! Cost is **not** stored in the transcript — it must be computed from
//! `usage × pricing` (see `specs/research-dossier.json`). The pricing/window
//! table is externalised as an embedded TOML file (`pricing.toml`, included at
//! compile time) so it can be updated without touching code.
//!
//! Window facts (web-verified, see CLAUDE.md / dossier): Opus 4.8/4.7/4.6 and
//! Sonnet 4.6 use a **1M** context window; Opus 4.5 / Sonnet 4.5 / Haiku 4.5
//! use **200k** (wrong denominator inflates ctx% ~5×). Prefer the
//! `context_window.context_window_size` reported by statusLine when present.
//!
//! The public surface here is consumed by the transcript collector, the
//! aggregator, and the statusLine fallback.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// Token counts for a single request's `message.usage`. All fields default to
/// `0` so partial/degraded usage objects still cost correctly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// 5-minute cache-creation tokens (`cache_creation.ephemeral_5m_input_tokens`).
    pub cache_create_5m: u64,
    /// 1-hour cache-creation tokens (`cache_creation.ephemeral_1h_input_tokens`).
    pub cache_create_1h: u64,
}

/// Per-million-token (`$/MTok`) prices plus the model's context window.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub context_window: u64,
}

/// Fallback window when a model id is unknown and statusLine gave no size.
/// 1M is the conservative choice for current frontier models — under-reporting
/// ctx% is far less misleading than the ~5× inflation a 200k denominator causes.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

#[derive(Debug, Deserialize)]
struct PricingTable {
    #[serde(rename = "model")]
    models: HashMap<String, ModelPricing>,
}

/// Embedded, easily-updatable pricing + context-window table (NFR-4).
const PRICING_TOML: &str = include_str!("pricing.toml");

fn table() -> &'static HashMap<String, ModelPricing> {
    static TABLE: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();
    TABLE.get_or_init(|| match toml::from_str::<PricingTable>(PRICING_TOML) {
        Ok(t) => t.models,
        Err(e) => {
            tracing::error!(error = %e, "embedded pricing.toml failed to parse; degrading to empty table");
            HashMap::new()
        }
    })
}

/// Look up pricing for a model id.
///
/// Live transcripts carry dated ids (e.g. `claude-haiku-4-5-20251001`), so we
/// match by longest known-key prefix before falling back to exact lookup. The
/// prefix must end on a delimiter boundary — the matched key is followed by `-`
/// or end-of-string — so `claude-opus-4-50` cannot inherit `claude-opus-4-5`'s
/// window/price (FR-6/AC-2).
pub fn pricing(model: &str) -> Option<ModelPricing> {
    let t = table();
    if let Some(p) = t.get(model) {
        return Some(*p);
    }
    t.iter()
        .filter(|(key, _)| matches_prefix(model, key.as_str()))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, p)| *p)
}

/// True when `key` is a delimiter-bounded prefix of `model`: `model` starts with
/// `key` and the next char (if any) is `-`. Guards against `claude-opus-4-5`
/// swallowing `claude-opus-4-50`.
fn matches_prefix(model: &str, key: &str) -> bool {
    model
        .strip_prefix(key)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// Context window (in tokens) for a model id, falling back to
/// [`DEFAULT_CONTEXT_WINDOW`] when the id is unknown.
pub fn context_window(model: &str) -> u64 {
    pricing(model)
        .map(|p| p.context_window)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

/// Effective context window: prefer the statusLine-provided `ctx_size` when
/// present (FR-3/AC-4), otherwise the per-model table value.
pub fn effective_context_window(model: &str, ctx_size: Option<u64>) -> u64 {
    ctx_size
        .filter(|&n| n > 0)
        .unwrap_or_else(|| context_window(model))
}

/// USD cost of a single request's usage for the given model.
///
/// `(in*P_in + out*P_out + cacheRead*P_cacheRead + cc5m*P_write5m + cc1h*P_write1h) / 1e6`
/// (formula verified in `specs/research-dossier.json`). Unknown model ⇒ `0.0`.
pub fn cost(model: &str, usage: &Usage) -> f64 {
    let Some(p) = pricing(model) else {
        return 0.0;
    };
    (usage.input as f64 * p.input
        + usage.output as f64 * p.output
        + usage.cache_read as f64 * p.cache_read
        + usage.cache_create_5m as f64 * p.cache_write_5m
        + usage.cache_create_1h as f64 * p.cache_write_1h)
        / 1_000_000.0
}

/// Live context-window fill as a percentage: `100 * live_ctx_tokens / window`.
///
/// `live_ctx_tokens` is the latest request's `input + cache_read +
/// cache_creation` (FR-2/AC-4); summing across the whole transcript overshoots
/// because `cache_read` accumulates per request. A non-positive window yields
/// `0.0` to avoid division by zero.
pub fn ctx_pct(live_ctx_tokens: u64, window: u64) -> f64 {
    if window == 0 {
        return 0.0;
    }
    100.0 * live_ctx_tokens as f64 / window as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worked example verified in specs/research-dossier.json:
    /// usage {input 131, output 12570, cache_read 59709, cc1h 10493} @ Opus 4.8
    /// = (131*5 + 12570*25 + 59709*0.5 + 10493*10)/1e6 ≈ $0.4497.
    #[test]
    fn cost_worked_example_opus_4_8() {
        let usage = Usage {
            input: 131,
            output: 12570,
            cache_read: 59709,
            cache_create_5m: 0,
            cache_create_1h: 10493,
        };
        let got = cost("claude-opus-4-8", &usage);
        assert!(
            (got - 0.449_690).abs() < 1e-6,
            "expected ≈ $0.4497, got ${got}"
        );
    }

    #[test]
    fn cost_matches_dossier_formula() {
        // Independently recompute the formula from the dossier and compare.
        let (i, o, cr, cc5m, cc1h) = (131.0, 12570.0, 59709.0, 0.0, 10493.0);
        let expected = (i * 5.0 + o * 25.0 + cr * 0.5 + cc5m * 6.25 + cc1h * 10.0) / 1e6;
        let usage = Usage {
            input: 131,
            output: 12570,
            cache_read: 59709,
            cache_create_5m: 0,
            cache_create_1h: 10493,
        };
        assert!((cost("claude-opus-4-8", &usage) - expected).abs() < 1e-9);
    }

    #[test]
    fn context_windows_match_verified_facts() {
        // 1M models.
        assert_eq!(context_window("claude-opus-4-8"), 1_000_000);
        assert_eq!(context_window("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window("claude-sonnet-4-6"), 1_000_000);
        // 200k models.
        assert_eq!(context_window("claude-opus-4-5"), 200_000);
        assert_eq!(context_window("claude-sonnet-4-5"), 200_000);
        assert_eq!(context_window("claude-haiku-4-5"), 200_000);
    }

    #[test]
    fn dated_model_ids_match_by_prefix() {
        // Live transcripts use dated ids (verified: claude-haiku-4-5-20251001).
        assert_eq!(context_window("claude-haiku-4-5-20251001"), 200_000);
        let usage = Usage {
            input: 1_000_000,
            ..Usage::default()
        };
        assert!((cost("claude-haiku-4-5-20251001", &usage) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn statusline_ctx_size_overrides_table() {
        // statusLine's context_window_size wins when present.
        assert_eq!(
            effective_context_window("claude-opus-4-5", Some(1_000_000)),
            1_000_000
        );
        // Absent / zero ⇒ fall back to the per-model window.
        assert_eq!(effective_context_window("claude-opus-4-5", None), 200_000);
        assert_eq!(
            effective_context_window("claude-opus-4-5", Some(0)),
            200_000
        );
    }

    #[test]
    fn prefix_match_requires_delimiter_boundary() {
        // A hypothetical future id must NOT inherit `claude-opus-4-5`'s 200k
        // window/price just because it shares the textual prefix (FR-6/AC-2).
        assert!(pricing("claude-opus-4-50").is_none());
        assert_eq!(context_window("claude-opus-4-50"), DEFAULT_CONTEXT_WINDOW);
        // Real models (exact + dated) still resolve correctly across the boundary.
        assert_eq!(context_window("claude-opus-4-8"), 1_000_000);
        assert_eq!(context_window("claude-opus-4-5"), 200_000);
        assert_eq!(context_window("claude-opus-4-5-20251001"), 200_000);
    }

    #[test]
    fn unknown_model_degrades_gracefully() {
        assert_eq!(cost("totally-unknown-model", &Usage::default()), 0.0);
        assert_eq!(
            context_window("totally-unknown-model"),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn ctx_pct_uses_correct_denominator() {
        // ~50.6% on 1M vs an impossible 252% on 200k (dossier example).
        let live = 505_765;
        assert!((ctx_pct(live, 1_000_000) - 50.5765).abs() < 1e-4);
        assert!((ctx_pct(live, 200_000) - 252.8825).abs() < 1e-4);
        // Division-by-zero guard.
        assert_eq!(ctx_pct(123, 0), 0.0);
    }

    #[test]
    fn embedded_pricing_toml_parses() {
        // A malformed embedded table degrades to empty at runtime (see `table`),
        // so guard the real file here: it must always parse at test time.
        assert!(toml::from_str::<PricingTable>(PRICING_TOML).is_ok());
    }

    #[test]
    fn sonnet_and_haiku_pricing_present() {
        // Spot-check the non-Opus rate rows load from the TOML.
        let usage = Usage {
            output: 1_000_000,
            ..Usage::default()
        };
        assert!((cost("claude-sonnet-4-6", &usage) - 15.0).abs() < 1e-9);
        assert!((cost("claude-haiku-4-5", &usage) - 5.0).abs() < 1e-9);
    }
}
