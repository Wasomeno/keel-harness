//! The live-run budget.
//!
//! Prices live here, not in the catalog: `SseDecoder` strips NeuralWatt's
//! `: cost {...}` SSE comment before protocol decoders see it, and the catalog
//! has no price fields. The live runner must have a cost source; if this one
//! were missing, it would refuse to run.
//!
//! # Price source
//!
//! NeuralWatt's pricing page on 2026-11-19 listed `deepseek-v4.1-flash` at
//! **$0.15 / 1M input tokens, $0.015 / 1M cached input tokens and $0.60 / 1M
//! output tokens** (the "listed token price"; energy-billed requests can come
//! in below it, never above it). Source: <https://neuralwatt.com/pricing>.
//!
//! The five committed `nw-small` live cassettes corroborate the shape: the
//! `: cost` comments report $0.000056 for 64 prompt / 35 output tokens, while
//! the listed `nw-small` price ($0.45 / $3.20 per 1M) gives $0.000141 as an
//! upper bound. The budget therefore charges listed prices, which is
//! conservative for both the pre-request worst case and the actual-spend
//! tally.

use keel_llm_types::Usage;

/// One million tokens, the denominator of every published rate.
const PER_MILLION: u64 = 1_000_000;

pub const MICROS_PER_DOLLAR: u64 = 1_000_000;

/// The hard cap for one live eval run: $10.00.
pub const LIVE_BUDGET_MICROS: u64 = 10 * MICROS_PER_DOLLAR;

pub const TOKEN_PRICE_SOURCE: &str =
    "NeuralWatt listed token prices, https://neuralwatt.com/pricing, read 2026-11-19";

/// Per-million-token prices in micro-USD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prices {
    pub input_micros_per_million: u64,
    pub cached_input_micros_per_million: u64,
    pub output_micros_per_million: u64,
}

/// NeuralWatt `deepseek-v4.1-flash`, listed prices.
pub const NEURALWATT_DEEPSEEK_V4_1_FLASH: Prices = Prices {
    input_micros_per_million: 150_000,       // $0.15
    cached_input_micros_per_million: 15_000, // $0.015
    output_micros_per_million: 600_000,      // $0.60
};

impl Prices {
    /// Worst-case cost of one request. Input is estimated as one token per
    /// serialized request byte (a deliberately safe upper bound for a
    /// byte-fallback tokenizer), and output is the session's `max_output_tokens`
    /// at the full output rate. Cached input is not assumed.
    pub fn worst_case_micros(&self, input_tokens: u64, max_output_tokens: u64) -> u64 {
        tokens_micros(input_tokens, self.input_micros_per_million).saturating_add(tokens_micros(
            max_output_tokens,
            self.output_micros_per_million,
        ))
    }

    /// Actual cost from reported usage. `input_tokens` already includes cached
    /// prompt tokens on OpenAI-compatible protocols, so charging the whole
    /// input at the fresh-input rate is conservative.
    pub fn actual_micros(&self, usage: &Usage) -> u64 {
        tokens_micros(usage.input_tokens, self.input_micros_per_million).saturating_add(
            tokens_micros(usage.output_tokens, self.output_micros_per_million),
        )
    }
}

/// Cost in micro-USD for `tokens` at `micros_per_million`, rounded up so a
/// fractional micro never understates a charge.
pub fn tokens_micros(tokens: u64, micros_per_million: u64) -> u64 {
    tokens
        .saturating_mul(micros_per_million)
        .div_ceil(PER_MILLION)
}

/// Spend so far plus a cap for one run. The runner asks [`allows`](Self::allows)
/// before every model request and only then runs the request; it never lets a
/// worst-case request cross the cap.
#[derive(Clone, Debug)]
pub struct Budget {
    prices: Prices,
    cap_micros: u64,
    spent_micros: u64,
}

impl Budget {
    pub fn new(prices: Prices, cap_micros: u64) -> Self {
        Self {
            prices,
            cap_micros,
            spent_micros: 0,
        }
    }

    pub fn prices(&self) -> Prices {
        self.prices
    }

    pub fn cap_micros(&self) -> u64 {
        self.cap_micros
    }

    pub fn spent_micros(&self) -> u64 {
        self.spent_micros
    }

    pub fn remaining_micros(&self) -> u64 {
        self.cap_micros.saturating_sub(self.spent_micros)
    }

    /// Whether a request with this input estimate and output limit fits. The
    /// check is `spent + worst_case <= cap`; equality is allowed.
    pub fn allows(&self, input_tokens: u64, max_output_tokens: u64) -> bool {
        self.spent_micros.saturating_add(
            self.prices
                .worst_case_micros(input_tokens, max_output_tokens),
        ) <= self.cap_micros
    }

    /// Adds what a completed turn actually reported.
    pub fn charge_usage(&mut self, usage: &Usage) {
        self.spent_micros = self
            .spent_micros
            .saturating_add(self.prices.actual_micros(usage));
    }

    #[cfg(test)]
    fn charge_micros(&mut self, amount: u64) {
        self.spent_micros = self.spent_micros.saturating_add(amount);
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new(NEURALWATT_DEEPSEEK_V4_1_FLASH, LIVE_BUDGET_MICROS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_prices_are_the_published_deepseek_v4_1_flash_rates() {
        assert_eq!(
            NEURALWATT_DEEPSEEK_V4_1_FLASH,
            Prices {
                input_micros_per_million: 150_000,
                cached_input_micros_per_million: 15_000,
                output_micros_per_million: 600_000,
            }
        );
        assert_eq!(LIVE_BUDGET_MICROS, 10 * MICROS_PER_DOLLAR);
    }

    #[test]
    fn a_worst_case_request_charges_full_input_and_the_output_limit() {
        let prices = NEURALWATT_DEEPSEEK_V4_1_FLASH;
        // 1M input tokens is $0.15; 1M output tokens is $0.60.
        assert_eq!(prices.worst_case_micros(1_000_000, 1_000_000), 750_000);
        assert_eq!(prices.worst_case_micros(0, 0), 0);
    }

    #[test]
    fn actual_spend_charges_input_and_output_once() {
        let prices = NEURALWATT_DEEPSEEK_V4_1_FLASH;
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 900_000,
            reasoning_tokens: 500_000,
            ..Usage::default()
        };
        assert_eq!(prices.actual_micros(&usage), 750_000);
    }

    #[test]
    fn a_request_at_the_cap_is_allowed_and_the_next_one_is_not() {
        let prices = Prices {
            input_micros_per_million: 1_000_000,
            cached_input_micros_per_million: 1_000_000,
            output_micros_per_million: 1_000_000,
        };
        let mut budget = Budget::new(prices, 20);
        // 10 input + 5 output = 15 micro-USD.
        assert!(budget.allows(10, 5));
        budget.charge_micros(5);
        assert!(
            budget.allows(10, 5),
            "spent 5 + worst 15 is exactly the cap"
        );
        budget.charge_micros(10);
        assert!(!budget.allows(10, 5), "spent 15 + worst 15 crosses the cap");
    }
}
