//! Offline, exact-model-id OpenAI API text-token price estimates.
//!
//! Rates are a documented snapshot, not live billing data. Neither the Codex
//! ChatGPT subscription nor the backend's actual service tier is observable
//! here. Apply Standard API prices only; do not guess Fast-tier charges.
//! No network requests, traffic relay, or writes to Codex files are involved.

use crate::usage::{TokenCounts, UsageRecord, UsageReport};
use serde::Serialize;
use std::collections::BTreeMap;

/// Published OpenAI API Standard token rates as checked on this date.
pub const PRICING_AS_OF: &str = "2026-10-08";

const LONG_CONTEXT_THRESHOLD: u64 = 272_000;
const PICO_USD_PER_USD: u128 = 1_000_000_000_000;

/// All rates are integer micro-USD per 1,000,000 tokens. The product of one
/// token and a rate has units of pico-USD, avoiding floating point errors.
#[derive(Debug, Clone, Copy)]
pub struct ModelRate {
    pub canonical_id: &'static str,
    pub input_micro_usd_per_mtok: u64,
    pub cached_micro_usd_per_mtok: u64,
    pub cache_write_micro_usd_per_mtok: u64,
    pub output_micro_usd_per_mtok: u64,
    pub long_context_surcharge: bool,
    pub source_url: &'static str,
}

impl ModelRate {
    const fn new(
        canonical_id: &'static str,
        input: u64,
        cached: u64,
        cache_write: u64,
        output: u64,
        long_context_surcharge: bool,
        source_url: &'static str,
    ) -> Self {
        Self {
            canonical_id,
            input_micro_usd_per_mtok: input,
            cached_micro_usd_per_mtok: cached,
            cache_write_micro_usd_per_mtok: cache_write,
            output_micro_usd_per_mtok: output,
            long_context_surcharge,
            source_url,
        }
    }
}

/// Exact, case-sensitive allow-list of documented API model IDs and snapshots.
/// Never infer the price of `gpt-5.4-unknown` from `gpt-5.4` or coerce a
/// future `gpt-6.1-*` variant to today's `gpt-6.1-sol` price.
pub fn rate_for_model(model_id: &str) -> Option<ModelRate> {
    const BASE: &str = "https://developers.openai.com/api/docs/models/";
    // Explicit full URLs are retained as audit references for every rate.
    // For models without published cache-write rates, use the uncached input
    // rate for tokens explicitly reported as cache writes.
    let rate = match model_id {
        "gpt-5.2" | "gpt-5.2-2025-12-11" => ModelRate::new(
            "gpt-5.2",
            1_750_000,
            175_000,
            1_750_000,
            14_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.2",
        ),
        "gpt-5.2-codex" => ModelRate::new(
            "gpt-5.2-codex",
            1_750_000,
            175_000,
            1_750_000,
            14_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.2-codex",
        ),
        "gpt-5.2-chat-latest" => ModelRate::new(
            "gpt-5.2-chat-latest",
            1_750_000,
            175_000,
            1_750_000,
            14_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.2-chat-latest",
        ),
        "gpt-5.2-pro" | "gpt-5.2-pro-2025-12-11" => ModelRate::new(
            "gpt-5.2-pro",
            21_000_000,
            21_000_000,
            21_000_000,
            168_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.2-pro",
        ),
        "gpt-5.3-codex" => ModelRate::new(
            "gpt-5.3-codex",
            1_750_000,
            175_000,
            1_750_000,
            14_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.3-codex",
        ),
        "gpt-5.3-chat-latest" => ModelRate::new(
            "gpt-5.3-chat-latest",
            1_750_000,
            175_000,
            1_750_000,
            14_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.3-chat-latest",
        ),
        "gpt-5.4" | "gpt-5.4-2026-03-05" => ModelRate::new(
            "gpt-5.4",
            2_500_000,
            250_000,
            2_500_000,
            15_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.4",
        ),
        "gpt-5.4-mini" | "gpt-5.4-mini-2026-03-17" => ModelRate::new(
            "gpt-5.4-mini",
            750_000,
            75_000,
            750_000,
            4_500_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.4-mini",
        ),
        "gpt-5.4-nano" | "gpt-5.4-nano-2026-03-17" => ModelRate::new(
            "gpt-5.4-nano",
            200_000,
            20_000,
            200_000,
            1_250_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.4-nano",
        ),
        "gpt-5.4-pro" | "gpt-5.4-pro-2026-03-05" => ModelRate::new(
            "gpt-5.4-pro",
            30_000_000,
            30_000_000,
            30_000_000,
            180_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.4-pro",
        ),
        "gpt-5.5" | "gpt-5.5-2026-04-23" => ModelRate::new(
            "gpt-5.5",
            5_000_000,
            500_000,
            5_000_000,
            30_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.5",
        ),
        "gpt-5.5-pro" | "gpt-5.5-pro-2026-04-23" => ModelRate::new(
            "gpt-5.5-pro",
            30_000_000,
            30_000_000,
            30_000_000,
            180_000_000,
            false,
            "https://developers.openai.com/api/docs/models/gpt-5.5-pro",
        ),
        // The documented gpt-5.6 alias routes to GPT-5.6 Sol. Its $4/M input
        // and $20/M output promotional rates are valid through at least
        // 2026-11-21; future price changes require a new catalog snapshot.
        "gpt-5.6" | "gpt-5.6-sol" => ModelRate::new(
            "gpt-5.6-sol",
            4_000_000,
            400_000,
            5_000_000,
            20_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.6-sol",
        ),
        "gpt-5.6-terra" => ModelRate::new(
            "gpt-5.6-terra",
            2_000_000,
            200_000,
            2_500_000,
            12_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.6-terra",
        ),
        "gpt-5.6-luna" => ModelRate::new(
            "gpt-5.6-luna",
            200_000,
            20_000,
            250_000,
            1_200_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.6-luna",
        ),
        "gpt-5.6-cyber" => ModelRate::new(
            "gpt-5.6-cyber",
            12_500_000,
            1_250_000,
            15_625_000,
            75_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-5.6-cyber",
        ),
        "gpt-6-astra" => ModelRate::new(
            "gpt-6-astra",
            10_000_000,
            1_000_000,
            12_500_000,
            50_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-6-astra",
        ),
        "gpt-6-sol" => ModelRate::new(
            "gpt-6-sol",
            2_000_000,
            200_000,
            2_500_000,
            10_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-6-sol",
        ),
        "gpt-6-luna" => ModelRate::new(
            "gpt-6-luna",
            100_000,
            10_000,
            125_000,
            500_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-6-luna",
        ),
        "gpt-6.1-sol" => ModelRate::new(
            "gpt-6.1-sol",
            2_000_000,
            100_000,
            2_500_000,
            10_000_000,
            true,
            "https://developers.openai.com/api/docs/models/gpt-6.1-sol",
        ),
        _ => return None,
    };
    debug_assert!(rate.source_url.starts_with(BASE));
    Some(rate)
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsePrice {
    /// Canonical ID of the exact-match pricing entry used, not a family prefix.
    pub price_model_id: String,
    pub price_source_url: String,
    pub price_as_of: String,
    pub long_context_surcharge: bool,
    pub standard_usd: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPriceSummary {
    pub model: String,
    pub responses: usize,
    pub standard_usd: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnpricedGroup {
    pub model: String,
    pub reason: String,
    pub responses: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PricingSummary {
    pub as_of: String,
    pub currency: String,
    pub unit: String,
    pub priced_responses: usize,
    pub unpriced_responses: usize,
    pub long_context_responses: usize,
    /// Total only over priced responses (not the entire archive if unpriced > 0).
    pub standard_usd: String,
    pub models: Vec<ModelPriceSummary>,
    pub unpriced: Vec<UnpricedGroup>,
}

impl Default for PricingSummary {
    fn default() -> Self {
        Self {
            as_of: PRICING_AS_OF.to_owned(),
            currency: "USD".to_owned(),
            unit: "OpenAI API Standard-equivalent text-token estimate".to_owned(),
            priced_responses: 0,
            unpriced_responses: 0,
            long_context_responses: 0,
            standard_usd: "0".to_owned(),
            models: Vec::new(),
            unpriced: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Accumulator {
    count: usize,
    standard: u128,
}

/// One pico-USD is exactly 1e-12 dollars; rates expressed as micro-USD/Mtok
/// multiply directly by a token count to yield pico-USD.
fn usd_from_pico(pico: u128) -> String {
    let dollars = pico / PICO_USD_PER_USD;
    let decimals = format!("{:012}", pico % PICO_USD_PER_USD);
    let decimals = decimals.trim_end_matches('0');
    if decimals.is_empty() {
        dollars.to_string()
    } else {
        format!("{dollars}.{decimals}")
    }
}

fn standard_pico(record: &UsageRecord, rate: &ModelRate) -> Option<(u128, bool)> {
    let TokenCounts {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        ..
    } = &record.tokens;
    if cached_input_tokens.saturating_add(*cache_write_input_tokens) > *input_tokens {
        return None;
    }
    if rate.long_context_surcharge && record.request_context_tokens.is_none() {
        // Delta-derived legacy tokens are not proof of the size of one prompt.
        // Do not silently omit a potentially expensive long-context premium.
        return None;
    }
    let fresh = input_tokens - cached_input_tokens - cache_write_input_tokens;
    let input_pico = u128::from(fresh) * u128::from(rate.input_micro_usd_per_mtok)
        + u128::from(*cached_input_tokens) * u128::from(rate.cached_micro_usd_per_mtok)
        + u128::from(*cache_write_input_tokens) * u128::from(rate.cache_write_micro_usd_per_mtok);
    let output_pico = u128::from(*output_tokens) * u128::from(rate.output_micro_usd_per_mtok);
    let long = rate.long_context_surcharge
        && record
            .request_context_tokens
            .is_some_and(|tokens| tokens > LONG_CONTEXT_THRESHOLD);
    if long {
        // The surcharge applies to the *entire request*, not just >272K.
        // Output is 1.5x; round fractional pico-USD to the nearest pico-USD.
        Some((input_pico * 2 + (output_pico * 3).div_ceil(2), true))
    } else {
        Some((input_pico + output_pico, false))
    }
}

pub(crate) fn fill_report_prices(report: &mut UsageReport) {
    let mut total = Accumulator::default();
    let mut long_context_responses = 0;
    let mut unpriced: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Accumulator> = BTreeMap::new();

    for session in &mut report.sessions {
        for record in &mut session.records {
            let Some(rate) = rate_for_model(&record.model) else {
                *unpriced
                    .entry((record.model.clone(), "unknown_exact_model_id".into()))
                    .or_default() += 1;
                continue;
            };
            let Some((standard, long)) = standard_pico(record, &rate) else {
                let reason = if record
                    .tokens
                    .cached_input_tokens
                    .saturating_add(record.tokens.cache_write_input_tokens)
                    > record.tokens.input_tokens
                {
                    "invalid_cache_breakdown"
                } else {
                    "missing_per_request_input_for_long_context"
                };
                *unpriced
                    .entry((record.model.clone(), reason.to_owned()))
                    .or_default() += 1;
                continue;
            };
            record.price = Some(ResponsePrice {
                price_model_id: rate.canonical_id.to_owned(),
                price_source_url: rate.source_url.to_owned(),
                price_as_of: PRICING_AS_OF.to_owned(),
                long_context_surcharge: long,
                standard_usd: usd_from_pico(standard),
            });
            total.count += 1;
            total.standard += standard;
            long_context_responses += usize::from(long);
            let model = by_model.entry(record.model.clone()).or_default();
            model.count += 1;
            model.standard += standard;
        }
    }

    report.pricing = PricingSummary {
        as_of: PRICING_AS_OF.to_owned(),
        currency: "USD".to_owned(),
        unit: "OpenAI API Standard-equivalent text-token estimate".to_owned(),
        priced_responses: total.count,
        unpriced_responses: unpriced.values().sum(),
        long_context_responses,
        standard_usd: usd_from_pico(total.standard),
        models: by_model
            .into_iter()
            .map(|(model, v)| ModelPriceSummary {
                model,
                responses: v.count,
                standard_usd: usd_from_pico(v.standard),
            })
            .collect(),
        unpriced: unpriced
            .into_iter()
            .map(|((model, reason), responses)| UnpricedGroup {
                model,
                reason,
                responses,
            })
            .collect(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageSource;

    fn sample(model: &str, input: u64, cached: u64, writes: u64, output: u64) -> UsageRecord {
        UsageRecord {
            source: UsageSource::TokenUsageRecord,
            model: model.into(),
            requested_service_tier: Some("default".into()),
            timestamp: None,
            response_id: None,
            turn_id: None,
            tokens: TokenCounts {
                input_tokens: input,
                cached_input_tokens: cached,
                cache_write_input_tokens: writes,
                output_tokens: output,
                reasoning_output_tokens: 0,
            },
            tool_calls: 0,
            tool_call_ids: Vec::new(),
            request_context_tokens: Some(input),
            model_context_window: None,
            price: None,
            dedup_id: String::new(),
        }
    }

    #[test]
    fn exact_ids_are_the_only_matches() {
        for id in [
            "gpt-5.2",
            "gpt-5.2-codex",
            "gpt-5.2-pro",
            "gpt-5.3-codex",
            "gpt-5.3-chat-latest",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.4-pro",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.6",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.6-cyber",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "gpt-5.4-mini-2026-03-17",
            "gpt-5.5-2026-04-23",
        ] {
            assert!(rate_for_model(id).is_some(), "missing {id}");
        }
        for id in [
            "gpt-5.1",
            "gpt-5.3",
            "gpt-5.3-codex-spark",
            "gpt-6.1",
            "gpt-6.1-astra",
            "gpt-6.2-sol",
            "gpt-6-astra-2027-01-01",
            "gpt-5.4-mini-preview",
            "GPT-6-SOL",
            "gpt-6-sol ",
            "unknown",
        ] {
            assert!(rate_for_model(id).is_none(), "must not match {id}");
        }
        assert_eq!(
            rate_for_model("gpt-5.6").unwrap().canonical_id,
            "gpt-5.6-sol"
        );
    }

    #[test]
    fn cached_tokens_and_cache_writes_are_not_double_counted() {
        let rec = sample("gpt-6.1-sol", 1_000_000, 600_000, 100_000, 200_000);
        let (pico, long) = standard_pico(&rec, &rate_for_model(&rec.model).unwrap()).unwrap();
        // 300k fresh * $2 + 600k cached * $0.10 + 100k write * $2.50
        // => $0.91 input; $2 output; long premium applies to ALL tokens.
        assert!(long);
        assert_eq!(usd_from_pico(pico), "4.82"); // 0.91*2 + 2*1.5
    }

    #[test]
    fn premium_starts_strictly_above_272k_for_enabled_models_only() {
        let small = sample("gpt-6-sol", 272_000, 0, 0, 100);
        let big = sample("gpt-6-sol", 272_001, 0, 0, 100);
        let rate = rate_for_model("gpt-6-sol").unwrap();
        assert!(!standard_pico(&small, &rate).unwrap().1);
        assert!(standard_pico(&big, &rate).unwrap().1);
        let mini = sample("gpt-5.4-mini", 300_000, 0, 0, 100);
        assert!(
            !standard_pico(&mini, &rate_for_model("gpt-5.4-mini").unwrap())
                .unwrap()
                .1
        );
    }

    #[test]
    fn cached_pro_tokens_do_not_get_discount() {
        let rec = sample("gpt-5.5-pro", 100_000, 80_000, 0, 20_000);
        let (pico, _) = standard_pico(&rec, &rate_for_model(&rec.model).unwrap()).unwrap();
        assert_eq!(usd_from_pico(pico), "6.6"); // 0.1*30 + 0.02*180
    }

    #[test]
    fn separate_old_and_new_cached_rates() {
        let sol = rate_for_model("gpt-6-sol").unwrap();
        let next = rate_for_model("gpt-6.1-sol").unwrap();
        assert_eq!(sol.cached_micro_usd_per_mtok, 200_000);
        assert_eq!(next.cached_micro_usd_per_mtok, 100_000);
        assert_eq!(next.cache_write_micro_usd_per_mtok, 2_500_000);
    }

    #[test]
    fn unknown_context_of_premium_model_is_not_free_of_surcharge() {
        let mut rec = sample("gpt-6-sol", 100, 0, 0, 10);
        rec.request_context_tokens = None;
        assert!(standard_pico(&rec, &rate_for_model("gpt-6-sol").unwrap()).is_none());
        let old = sample("gpt-5.2", 100, 0, 0, 10);
        assert!(standard_pico(&old, &rate_for_model("gpt-5.2").unwrap()).is_some());
    }
}
