use serde::{Deserialize, Serialize};

use super::UsdAmount;
use crate::Usage;

// OpenAI publishes rates per one million tokens. All supported rates convert
// exactly to nano-USD per token, avoiding floating point and division.
const STANDARD: TokenRates = TokenRates {
    input: 5_000,
    cached_input: 500,
    cache_write_input: 6_250,
    output: 30_000,
};
const PRIORITY: TokenRates = TokenRates {
    input: 10_000,
    cached_input: 1_000,
    cache_write_input: 12_500,
    output: 60_000,
};

#[derive(Clone, Copy)]
struct TokenRates {
    input: u64,
    cached_input: u64,
    cache_write_input: u64,
    output: u64,
}

impl TokenRates {
    const fn for_service_tier(service_tier: ServiceTier) -> Self {
        match service_tier {
            ServiceTier::Standard => STANDARD,
            ServiceTier::Priority => PRIORITY,
        }
    }
}

/// OpenAI service tiers supported by Nanocodex.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Standard processing and token rates.
    #[default]
    Standard,
    /// Priority processing selected by `fast_mode`.
    Priority,
}

impl ServiceTier {
    /// Returns the OpenAI service-tier name used in events and traces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Priority => "priority",
        }
    }
}

/// Exact estimated USD cost for provider-reported token usage.
///
/// Nanocodex calculates this automatically using the documented
/// [`crate::MODEL`] standard or priority rates. This is a local estimate, not a
/// charge reported by the Responses API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EstimatedUsdCost {
    #[serde(rename = "usd")]
    amount: UsdAmount,
    #[serde(rename = "input_usd")]
    input: UsdAmount,
    #[serde(rename = "cached_input_usd")]
    cached_input: UsdAmount,
    #[serde(rename = "cache_write_input_usd")]
    cache_write_input: UsdAmount,
    #[serde(rename = "output_usd")]
    output: UsdAmount,
    #[serde(default)]
    service_tier: ServiceTier,
}

impl EstimatedUsdCost {
    /// Returns the exact aggregate estimate.
    #[must_use]
    pub const fn amount(&self) -> UsdAmount {
        self.amount
    }

    /// Returns the ordinary-input component.
    #[must_use]
    pub const fn input(&self) -> UsdAmount {
        self.input
    }

    /// Returns the cache-read component.
    #[must_use]
    pub const fn cached_input(&self) -> UsdAmount {
        self.cached_input
    }

    /// Returns the cache-write component.
    #[must_use]
    pub const fn cache_write_input(&self) -> UsdAmount {
        self.cache_write_input
    }

    /// Returns the output component, including reasoning output.
    #[must_use]
    pub const fn output(&self) -> UsdAmount {
        self.output
    }

    /// Returns the service tier whose built-in rates were applied.
    #[must_use]
    pub const fn service_tier(&self) -> ServiceTier {
        self.service_tier
    }
}

/// Estimates one provider operation from its authoritative usage record.
///
/// Cached and cache-write tokens are subsets of `input_tokens`; this function
/// subtracts both before pricing ordinary input. The returned value is a local
/// estimate, not a charge reported by the Responses API.
///
/// ```
/// use nanocodex_oai_api::{
///     pricing::{ServiceTier, estimate},
///     responses::{InputTokenDetails, Usage},
/// };
///
/// let usage = Usage {
///     input_tokens: 1_000,
///     input_tokens_details: Some(InputTokenDetails {
///         cached_tokens: 800,
///         cache_write_tokens: 100,
///     }),
///     output_tokens: 50,
///     total_tokens: 1_050,
///     ..Usage::default()
/// };
/// let cost = estimate(&usage, ServiceTier::Standard);
///
/// assert_eq!(cost.amount().decimal(), "0.003025");
/// ```
#[must_use]
pub fn estimate(usage: &Usage, service_tier: ServiceTier) -> EstimatedUsdCost {
    let cached_input_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    let cache_write_input_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cache_write_tokens);
    estimate_tokens(
        usage.input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        usage.output_tokens,
        service_tier,
    )
}

pub(crate) fn estimate_tokens(
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    service_tier: ServiceTier,
) -> EstimatedUsdCost {
    let rates = TokenRates::for_service_tier(service_tier);
    let cached_input_tokens = cached_input_tokens.min(input_tokens);
    let remaining_input = input_tokens.saturating_sub(cached_input_tokens);
    let cache_write_input_tokens = cache_write_input_tokens.min(remaining_input);
    let ordinary_input_tokens = remaining_input.saturating_sub(cache_write_input_tokens);

    let input = UsdAmount::saturating_mul(ordinary_input_tokens, rates.input);
    let cached_input = UsdAmount::saturating_mul(cached_input_tokens, rates.cached_input);
    let cache_write_input =
        UsdAmount::saturating_mul(cache_write_input_tokens, rates.cache_write_input);
    let output = UsdAmount::saturating_mul(output_tokens, rates.output);
    let amount = input
        .saturating_add(cached_input)
        .saturating_add(cache_write_input)
        .saturating_add(output);

    EstimatedUsdCost {
        amount,
        input,
        cached_input,
        cache_write_input,
        output,
        service_tier,
    }
}

#[cfg(test)]
mod tests {
    use super::{ServiceTier, estimate, estimate_tokens};
    use crate::responses::{InputTokenDetails, OutputTokenDetails, Usage};

    #[test]
    fn standard_rates_price_each_input_class_once() {
        let estimate = estimate(
            &Usage {
                input_tokens: 1_000_000,
                input_tokens_details: Some(InputTokenDetails {
                    cached_tokens: 250_000,
                    cache_write_tokens: 100_000,
                }),
                output_tokens: 200_000,
                output_tokens_details: Some(OutputTokenDetails {
                    reasoning_tokens: 150_000,
                }),
                total_tokens: 1_200_000,
            },
            ServiceTier::Standard,
        );

        assert_eq!(estimate.input().decimal(), "3.25");
        assert_eq!(estimate.cached_input().decimal(), "0.125");
        assert_eq!(estimate.cache_write_input().decimal(), "0.625");
        assert_eq!(estimate.output().decimal(), "6");
        assert_eq!(estimate.amount().decimal(), "10");
    }

    #[test]
    fn priority_rates_are_selected_by_fast_mode() {
        let standard = estimate_tokens(1_000_000, 0, 0, 1_000_000, ServiceTier::Standard);
        let priority = estimate_tokens(1_000_000, 0, 0, 1_000_000, ServiceTier::Priority);

        assert_eq!(standard.amount().decimal(), "35");
        assert_eq!(priority.amount().decimal(), "70");
        assert_eq!(priority.service_tier(), ServiceTier::Priority);
        assert_eq!(priority.service_tier().as_str(), "priority");
    }

    #[test]
    fn malformed_detail_counts_do_not_double_charge_input() {
        let estimate = estimate_tokens(10, 8, 8, 0, ServiceTier::Standard);

        assert_eq!(estimate.input().nano_usd(), 0);
        assert_eq!(estimate.cached_input().nano_usd(), 4_000);
        assert_eq!(estimate.cache_write_input().nano_usd(), 12_500);
    }
}
