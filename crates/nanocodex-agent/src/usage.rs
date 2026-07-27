use serde::{Deserialize, Serialize};

/// Exact token accounting for every Responses call in one logical agent turn.
///
/// Cache-read and cache-write tokens are subsets of input tokens. Reasoning
/// tokens are a subset of output tokens. The values are summed from provider
/// usage records across warmup, generation, tool continuation, steering, and
/// compaction calls made before the turn reaches its terminal boundary. Check
/// [`Self::cost_status`] to distinguish a provider-omitted usage record from a
/// genuine zero-token total.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_field_names)]
pub struct TurnUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    estimated_cost: Option<Box<nanocodex_oai_api::EstimatedUsdCost>>,
    cost_status: nanocodex_oai_api::CostStatus,
}

#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy)]
pub(crate) struct TurnUsageCounts {
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_write_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) reported: bool,
}

impl TurnUsage {
    pub(crate) fn from_counts(
        counts: TurnUsageCounts,
        pricing: Option<&nanocodex_oai_api::PricingSnapshot>,
    ) -> Self {
        let (estimated_cost, cost_status) = match (pricing, counts.reported) {
            (Some(pricing), true) => (
                Some(Box::new(pricing.estimate_tokens(
                    counts.input_tokens,
                    counts.cached_input_tokens,
                    counts.cache_write_input_tokens,
                    counts.output_tokens,
                ))),
                nanocodex_oai_api::CostStatus::EstimatedFromUsage,
            ),
            (Some(_), false) => (None, nanocodex_oai_api::CostStatus::UsageNotReported),
            (None, _) => (None, nanocodex_oai_api::CostStatus::PricingNotConfigured),
        };
        Self {
            input_tokens: counts.input_tokens,
            cached_input_tokens: counts.cached_input_tokens,
            cache_write_input_tokens: counts.cache_write_input_tokens,
            output_tokens: counts.output_tokens,
            reasoning_output_tokens: counts.reasoning_output_tokens,
            total_tokens: counts.total_tokens,
            estimated_cost,
            cost_status,
        }
    }

    /// Returns all input tokens billed or reported by the provider.
    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Returns input tokens served from the provider's prompt cache.
    #[must_use]
    pub const fn cached_input_tokens(&self) -> u64 {
        self.cached_input_tokens
    }

    /// Returns input tokens newly written into the provider's prompt cache.
    #[must_use]
    pub const fn cache_write_input_tokens(&self) -> u64 {
        self.cache_write_input_tokens
    }

    /// Returns all output tokens billed or reported by the provider.
    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    /// Returns reasoning tokens included within [`Self::output_tokens`].
    #[must_use]
    pub const fn reasoning_output_tokens(&self) -> u64 {
        self.reasoning_output_tokens
    }

    /// Returns the provider-reported total token count.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// Returns the local USD estimate when a pricing snapshot was configured.
    ///
    /// The estimate includes its exact rates, source, and effective date.
    /// `None` means pricing was not configured or the provider omitted usage.
    /// Inspect [`Self::cost_status`] for the exact reason; absence is never
    /// serialized as a misleading zero.
    #[must_use]
    pub fn estimated_cost(&self) -> Option<&nanocodex_oai_api::EstimatedUsdCost> {
        self.estimated_cost.as_deref()
    }

    /// Returns why an estimate is present or unavailable.
    #[must_use]
    pub const fn cost_status(&self) -> nanocodex_oai_api::CostStatus {
        self.cost_status
    }
}
