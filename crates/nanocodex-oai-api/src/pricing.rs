use std::{fmt, str::FromStr, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{MODEL, Usage};

const NANO_USD_PER_USD: u64 = 1_000_000_000;
const TOKENS_PER_MILLION: u128 = 1_000_000;

/// Availability of a local USD estimate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CostStatus {
    /// An explicit pricing snapshot was applied to provider usage.
    EstimatedFromUsage,
    /// The application did not configure pricing.
    #[default]
    PricingNotConfigured,
    /// Pricing was configured but the provider omitted usage.
    UsageNotReported,
    /// A retained record used an older or unknown status.
    #[serde(other)]
    Other,
}

impl CostStatus {
    /// Returns the stable snake-case wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EstimatedFromUsage => "estimated_from_usage",
            Self::PricingNotConfigured => "pricing_not_configured",
            Self::UsageNotReported => "usage_not_reported",
            Self::Other => "other",
        }
    }
}

/// An exact non-negative amount of United States dollars.
///
/// The value is stored as billionths of one dollar and serialized as a decimal
/// string, so language bindings and durable eval artifacts never round through
/// a JSON floating-point number.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct UsdAmount {
    nano_usd: u64,
}

impl UsdAmount {
    /// Creates an amount from billionths of one dollar.
    #[must_use]
    pub const fn from_nano_usd(nano_usd: u64) -> Self {
        Self { nano_usd }
    }

    /// Returns the exact amount in billionths of one dollar.
    #[must_use]
    pub const fn nano_usd(self) -> u64 {
        self.nano_usd
    }

    /// Returns a decimal USD string without a currency symbol.
    #[must_use]
    pub fn decimal(self) -> String {
        format_decimal_usd(self.nano_usd)
    }

    /// Returns a floating-point projection for compatibility adapters.
    ///
    /// Use [`Self::nano_usd`] or [`Self::decimal`] for accounting and durable
    /// storage.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        self.nano_usd as f64 / NANO_USD_PER_USD as f64
    }

    const fn saturating_add(self, other: Self) -> Self {
        Self {
            nano_usd: self.nano_usd.saturating_add(other.nano_usd),
        }
    }
}

impl fmt::Display for UsdAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "${}", self.decimal())
    }
}

impl Serialize for UsdAmount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.decimal())
    }
}

impl<'de> Deserialize<'de> for UsdAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_decimal_usd(&value)
            .map(Self::from_nano_usd)
            .map_err(de::Error::custom)
    }
}

/// Exact USD rate for one million tokens in a single billing class.
///
/// Parse a plain decimal USD value such as `"1.25"` or `"0.125"`.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct UsdPerMillionTokens {
    nano_usd: u64,
}

impl UsdPerMillionTokens {
    /// Creates a rate from billionths of one USD per million tokens.
    #[must_use]
    pub const fn from_nano_usd(nano_usd: u64) -> Self {
        Self { nano_usd }
    }

    /// Returns billionths of one USD per million tokens.
    #[must_use]
    pub const fn nano_usd(self) -> u64 {
        self.nano_usd
    }

    /// Returns the rate as a decimal USD string.
    #[must_use]
    pub fn decimal(self) -> String {
        format_decimal_usd(self.nano_usd)
    }

    fn estimate(self, tokens: u64) -> UsdAmount {
        let numerator = u128::from(tokens).saturating_mul(u128::from(self.nano_usd));
        let rounded = numerator.saturating_add(TOKENS_PER_MILLION / 2) / TOKENS_PER_MILLION;
        UsdAmount::from_nano_usd(u64::try_from(rounded).unwrap_or(u64::MAX))
    }
}

impl FromStr for UsdPerMillionTokens {
    type Err = UsdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_decimal_usd(value).map(Self::from_nano_usd)
    }
}

impl fmt::Display for UsdPerMillionTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.decimal())
    }
}

impl Serialize for UsdPerMillionTokens {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.decimal())
    }
}

impl<'de> Deserialize<'de> for UsdPerMillionTokens {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Per-million-token rates used by one immutable pricing snapshot.
///
/// Provider input usage includes cached and cache-write tokens. Estimation
/// subtracts those two classes before applying `input`, so every reported
/// input token is priced exactly once. Reasoning tokens remain part of
/// `output` and are not charged a second time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenRates {
    /// Ordinary input tokens, in USD per million tokens.
    #[serde(rename = "input_usd_per_million")]
    pub input: UsdPerMillionTokens,
    /// Cache-read input tokens, in USD per million tokens.
    #[serde(rename = "cached_input_usd_per_million")]
    pub cached_input: UsdPerMillionTokens,
    /// Newly cached input tokens, in USD per million tokens.
    #[serde(rename = "cache_write_input_usd_per_million")]
    pub cache_write_input: UsdPerMillionTokens,
    /// Output tokens, including reasoning tokens, in USD per million tokens.
    #[serde(rename = "output_usd_per_million")]
    pub output: UsdPerMillionTokens,
}

/// Immutable, auditable pricing input for the one supported model contract.
///
/// The Responses API reports token usage rather than billed dollars.
/// Nanocodex therefore estimates cost only when an application explicitly
/// supplies one of these snapshots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PricingSnapshot {
    id: Arc<str>,
    source: Arc<str>,
    effective_date: Arc<str>,
    model: Arc<str>,
    rates: TokenRates,
}

impl PricingSnapshot {
    /// Creates a validated pricing snapshot for [`MODEL`].
    ///
    /// ```
    /// use nanocodex_oai_api::{
    ///     PricingSnapshot, TokenRates, UsdPerMillionTokens,
    /// };
    ///
    /// let pricing = PricingSnapshot::new(
    ///     "team-contract-2026-q3",
    ///     "https://billing.example.com/openai/2026-q3",
    ///     "2026-07-01",
    ///     TokenRates {
    ///         input: "1.25".parse::<UsdPerMillionTokens>()?,
    ///         cached_input: "0.125".parse::<UsdPerMillionTokens>()?,
    ///         cache_write_input: "1.25".parse::<UsdPerMillionTokens>()?,
    ///         output: "10.00".parse::<UsdPerMillionTokens>()?,
    ///     },
    /// )?;
    ///
    /// assert_eq!(pricing.effective_date(), "2026-07-01");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when identity or source is empty, or when the effective
    /// date is not an ISO `YYYY-MM-DD` calendar date.
    pub fn new(
        id: impl Into<Arc<str>>,
        source: impl Into<Arc<str>>,
        effective_date: impl Into<Arc<str>>,
        rates: TokenRates,
    ) -> Result<Self, PricingError> {
        Self::for_model(id, source, effective_date, MODEL, rates)
    }

    fn for_model(
        id: impl Into<Arc<str>>,
        source: impl Into<Arc<str>>,
        effective_date: impl Into<Arc<str>>,
        model: impl Into<Arc<str>>,
        rates: TokenRates,
    ) -> Result<Self, PricingError> {
        let snapshot = Self {
            id: id.into(),
            source: source.into(),
            effective_date: effective_date.into(),
            model: model.into(),
            rates,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Returns the stable application-selected pricing version.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the URL or durable identifier from which the rates came.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the ISO date on which these rates became effective.
    #[must_use]
    pub fn effective_date(&self) -> &str {
        &self.effective_date
    }

    /// Returns the model contract these rates cover.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the exact per-million-token rates.
    #[must_use]
    pub const fn rates(&self) -> TokenRates {
        self.rates
    }

    /// Estimates one provider operation from its authoritative usage record.
    #[must_use]
    pub fn estimate(&self, usage: &Usage) -> EstimatedUsdCost {
        let cached_input_tokens = usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens);
        let cache_write_input_tokens = usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cache_write_tokens);
        self.estimate_tokens(
            usage.input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            usage.output_tokens,
        )
    }

    /// Estimates aggregate token counts spanning multiple provider operations.
    #[must_use]
    pub fn estimate_tokens(
        &self,
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_write_input_tokens: u64,
        output_tokens: u64,
    ) -> EstimatedUsdCost {
        let cached_input_tokens = cached_input_tokens.min(input_tokens);
        let remaining_input = input_tokens.saturating_sub(cached_input_tokens);
        let cache_write_input_tokens = cache_write_input_tokens.min(remaining_input);
        let ordinary_input_tokens = remaining_input.saturating_sub(cache_write_input_tokens);

        let input = self.rates.input.estimate(ordinary_input_tokens);
        let cached_input = self.rates.cached_input.estimate(cached_input_tokens);
        let cache_write_input = self
            .rates
            .cache_write_input
            .estimate(cache_write_input_tokens);
        let output = self.rates.output.estimate(output_tokens);
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
            pricing: self.clone(),
        }
    }

    fn validate(&self) -> Result<(), PricingError> {
        if self.id.trim().is_empty() {
            return Err(PricingError::EmptyId);
        }
        if self.source.trim().is_empty() {
            return Err(PricingError::EmptySource);
        }
        if self.model.as_ref() != MODEL {
            return Err(PricingError::UnsupportedModel {
                model: self.model.to_string(),
            });
        }
        if !is_iso_date(&self.effective_date) {
            return Err(PricingError::InvalidEffectiveDate {
                value: self.effective_date.to_string(),
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for PricingSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: Arc<str>,
            source: Arc<str>,
            effective_date: Arc<str>,
            model: Arc<str>,
            rates: TokenRates,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::for_model(
            wire.id,
            wire.source,
            wire.effective_date,
            wire.model,
            wire.rates,
        )
        .map_err(de::Error::custom)
    }
}

/// Exact estimated USD cost plus its complete pricing provenance.
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
    pricing: PricingSnapshot,
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

    /// Returns the exact rates and provenance used for the estimate.
    #[must_use]
    pub const fn pricing(&self) -> &PricingSnapshot {
        &self.pricing
    }
}

/// Invalid decimal USD input.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UsdParseError {
    /// The amount was empty.
    #[error("USD amount must not be empty")]
    Empty,
    /// The amount was not a plain non-negative decimal.
    #[error("invalid USD amount {value:?}; expected a non-negative decimal")]
    Invalid {
        /// Rejected input.
        value: String,
    },
    /// More than nine decimal places were supplied.
    #[error("USD amount {value:?} has more than nine decimal places")]
    TooPrecise {
        /// Rejected input.
        value: String,
    },
    /// The exact amount exceeded the supported range.
    #[error("USD amount {value:?} is too large")]
    Overflow {
        /// Rejected input.
        value: String,
    },
}

/// Invalid pricing snapshot metadata.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PricingError {
    /// Snapshot identity was empty.
    #[error("pricing snapshot id must not be empty")]
    EmptyId,
    /// Pricing provenance was empty.
    #[error("pricing snapshot source must not be empty")]
    EmptySource,
    /// The date was not an ISO calendar date.
    #[error("invalid pricing effective date {value:?}; expected YYYY-MM-DD")]
    InvalidEffectiveDate {
        /// Rejected date.
        value: String,
    },
    /// The snapshot targeted a model outside this SDK's fixed contract.
    #[error("pricing snapshot targets unsupported model {model:?}; expected {MODEL:?}")]
    UnsupportedModel {
        /// Rejected model.
        model: String,
    },
}

fn parse_decimal_usd(value: &str) -> Result<u64, UsdParseError> {
    if value.is_empty() {
        return Err(UsdParseError::Empty);
    }
    let (whole, fractional) = value.split_once('.').map_or((value, ""), |parts| parts);
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(UsdParseError::Invalid {
            value: value.to_owned(),
        });
    }
    if fractional.len() > 9 {
        return Err(UsdParseError::TooPrecise {
            value: value.to_owned(),
        });
    }
    let fractional_digits =
        u32::try_from(fractional.len()).map_err(|_| UsdParseError::Overflow {
            value: value.to_owned(),
        })?;
    let whole = whole.parse::<u64>().map_err(|_| UsdParseError::Overflow {
        value: value.to_owned(),
    })?;
    let fractional = if fractional.is_empty() {
        0
    } else {
        fractional
            .parse::<u64>()
            .map_err(|_| UsdParseError::Overflow {
                value: value.to_owned(),
            })?
            .checked_mul(10_u64.pow(9_u32.saturating_sub(fractional_digits)))
            .ok_or_else(|| UsdParseError::Overflow {
                value: value.to_owned(),
            })?
    };
    whole
        .checked_mul(NANO_USD_PER_USD)
        .and_then(|whole| whole.checked_add(fractional))
        .ok_or_else(|| UsdParseError::Overflow {
            value: value.to_owned(),
        })
}

fn format_decimal_usd(nano_usd: u64) -> String {
    let whole = nano_usd / NANO_USD_PER_USD;
    let fractional = nano_usd % NANO_USD_PER_USD;
    if fractional == 0 {
        return whole.to_string();
    }
    let fractional = format!("{fractional:09}");
    format!("{whole}.{}", fractional.trim_end_matches('0'))
}

fn is_iso_date(value: &str) -> bool {
    if value.len() != 10 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let year = value[..4].parse::<u16>().unwrap_or_default();
    let month = value[5..7].parse::<u8>().unwrap_or_default();
    let day = value[8..].parse::<u8>().unwrap_or_default();
    year != 0 && (1..=12).contains(&month) && day != 0 && day <= days_in_month(year, month)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{InputTokenDetails, OutputTokenDetails};

    fn pricing() -> PricingSnapshot {
        PricingSnapshot::new(
            "contract-v1",
            "billing-contract",
            "2026-07-01",
            TokenRates {
                input: "1.25".parse().unwrap(),
                cached_input: "0.125".parse().unwrap(),
                cache_write_input: "1.50".parse().unwrap(),
                output: "10".parse().unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn decimal_usd_is_exact_and_canonical() {
        let rate = "0.000000001".parse::<UsdPerMillionTokens>().unwrap();
        assert_eq!(rate.nano_usd(), 1);
        assert_eq!(rate.to_string(), "0.000000001");
        assert_eq!(
            "1.0000000001".parse::<UsdPerMillionTokens>(),
            Err(UsdParseError::TooPrecise {
                value: "1.0000000001".to_owned()
            })
        );
        assert!("1e-3".parse::<UsdPerMillionTokens>().is_err());
        assert!("-1".parse::<UsdPerMillionTokens>().is_err());
    }

    #[test]
    fn estimate_prices_each_input_class_once_and_reasoning_as_output() {
        let estimate = pricing().estimate(&Usage {
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
        });

        assert_eq!(estimate.input().decimal(), "0.8125");
        assert_eq!(estimate.cached_input().decimal(), "0.03125");
        assert_eq!(estimate.cache_write_input().decimal(), "0.15");
        assert_eq!(estimate.output().decimal(), "2");
        assert_eq!(estimate.amount().decimal(), "2.99375");
    }

    #[test]
    fn snapshot_json_is_human_readable_validated_and_round_trips() {
        let pricing = pricing();
        let value = serde_json::to_value(&pricing).unwrap();
        assert_eq!(value["model"], MODEL);
        assert_eq!(value["rates"]["input_usd_per_million"], "1.25");
        assert_eq!(
            serde_json::from_value::<PricingSnapshot>(value).unwrap(),
            pricing
        );

        let wrong_model = json!({
            "id": "contract-v1",
            "source": "billing-contract",
            "effective_date": "2026-07-01",
            "model": "different-model",
            "rates": {
                "input_usd_per_million": "1",
                "cached_input_usd_per_million": "1",
                "cache_write_input_usd_per_million": "1",
                "output_usd_per_million": "1"
            }
        });
        assert!(serde_json::from_value::<PricingSnapshot>(wrong_model).is_err());
    }

    #[test]
    fn effective_date_rejects_non_calendar_dates() {
        assert_eq!(
            PricingSnapshot::new(
                "contract-v1",
                "billing-contract",
                "2026-02-29",
                pricing().rates()
            ),
            Err(PricingError::InvalidEffectiveDate {
                value: "2026-02-29".to_owned()
            })
        );
        PricingSnapshot::new(
            "contract-v1",
            "billing-contract",
            "2028-02-29",
            pricing().rates(),
        )
        .unwrap();
    }

    #[test]
    fn cost_status_has_stable_wire_names_and_retains_unknown_values() {
        assert_eq!(
            serde_json::to_value(CostStatus::EstimatedFromUsage).unwrap(),
            json!("estimated_from_usage")
        );
        assert_eq!(
            serde_json::from_value::<CostStatus>(json!("usage_not_reported")).unwrap(),
            CostStatus::UsageNotReported
        );
        assert_eq!(
            serde_json::from_value::<CostStatus>(json!("future_status")).unwrap(),
            CostStatus::Other
        );
        assert_eq!(
            CostStatus::PricingNotConfigured.as_str(),
            "pricing_not_configured"
        );
    }
}
