//! What a session has spent: dollars, tokens, and usage the price table could not price yet.

use crate::auth::chatgpt;
use crate::model::pricing;
use mu_base::message::Usage;

/// `provider/model` as the price table keys it. Assistant messages from mu carry the full id; omp's carry
/// the bare model id next to the provider.
pub fn priced_id(provider: &str, model: &str) -> String {
    if model.contains('/') { model.to_string() } else { format!("{provider}/{model}") }
}

fn has_tokens(u: &Usage) -> bool {
    u.input + u.output + u.cache_read + u.cache_write > 0
}

#[derive(Default)]
pub struct CostLedger {
    /// Session spend in dollars.
    pub total: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Usage the price table could not price yet (`provider/model`, usage). Priced into `total` once
    /// the table knows the model; dropped if the table lists it without a price.
    unpriced: Vec<(String, Usage)>,
}

impl CostLedger {
    /// Records one assistant response, pricing it now if the table knows the model. `usage.cost`
    /// is filled in when priced.
    pub fn record(&mut self, provider: &str, model: &str, usage: &mut Usage) {
        // The ChatGPT plan is not billed per token: its tokens count, its dollars never do.
        if provider != chatgpt::PROVIDER_ID {
            let id = priced_id(provider, model);
            match pricing::cost(&id, usage) {
                Some(c) => usage.cost = c,
                None if has_tokens(usage) => self.unpriced.push((id, usage.clone())),
                None => {}
            }
            self.total += usage.cost.total;
        }
        self.tokens_in += usage.input + usage.cache_read + usage.cache_write;
        self.tokens_out += usage.output;
    }

    /// Takes in a stored response when a session is resumed: its recorded cost, except turns that
    /// were saved unpriced (the price table was missing the model at the time).
    pub fn add_stored(&mut self, provider: &str, model: &str, usage: &Usage) {
        if provider == chatgpt::PROVIDER_ID {
            return;
        }
        if usage.cost.total == 0.0 && has_tokens(usage) {
            self.unpriced.push((priced_id(provider, model), usage.clone()));
        } else {
            self.total += usage.cost.total;
        }
    }

    /// Prices usage recorded before the table knew its model. Returns true when some model is still
    /// missing from the table, so the caller can force one catalogue refresh.
    pub fn settle(&mut self) -> bool {
        if self.unpriced.is_empty() {
            return false;
        }
        let mut missing = false;
        let mut added = 0.0;
        self.unpriced.retain(|(id, usage)| match pricing::cost(id, usage) {
            Some(c) => {
                added += c.total;
                false
            }
            None if pricing::known(id) => false,
            None => {
                missing = true;
                true
            }
        });
        self.total += added;
        missing
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether any usage still waits for a price.
    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.unpriced.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_token_counts() {
        assert_eq!(priced_id("anthropic", "claude-x"), "anthropic/claude-x");
        assert_eq!(priced_id("anthropic", "anthropic/claude-x"), "anthropic/claude-x");
        let mut l = CostLedger::default();
        let mut u = Usage { input: 10, output: 5, cache_read: 3, cache_write: 2, ..Default::default() };
        l.record("nobody", "no-such-model", &mut u);
        assert_eq!((l.tokens_in, l.tokens_out), (15, 5));
        assert_eq!(l.total, 0.0);
        assert_eq!(l.pending(), 1, "an unknown model waits for a price");
        l.reset();
        assert_eq!((l.tokens_in, l.pending()), (0, 0));
    }

    #[test]
    fn the_chatgpt_plan_counts_tokens_and_never_dollars() {
        let mut l = CostLedger::default();
        let mut u = Usage { input: 10, output: 5, ..Default::default() };
        l.record(chatgpt::PROVIDER_ID, "gpt-6.1-sol", &mut u);
        l.add_stored(chatgpt::PROVIDER_ID, "gpt-6.1-sol", &u);
        assert_eq!((l.tokens_in, l.tokens_out, l.total, l.pending()), (10, 5, 0.0, 0));
    }

    #[test]
    fn stored_unpriced_turns_wait_and_priced_ones_add_up() {
        let mut l = CostLedger::default();
        let priced = Usage {
            input: 1,
            cost: mu_base::message::Cost { total: 0.25, ..Default::default() },
            ..Default::default()
        };
        let unpriced = Usage { input: 4, ..Default::default() };
        l.add_stored("p", "m", &priced);
        l.add_stored("p", "m", &unpriced);
        assert_eq!(l.total, 0.25);
        assert_eq!(l.pending(), 1);
    }
}
