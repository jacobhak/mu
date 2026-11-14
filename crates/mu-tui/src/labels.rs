//! Display words for provider data, shared by the terminal UI and the `mu providers` commands.

use mu_provider::provider::custom::{Cost, KeyState};

/// The short word picker rows and status lines show for a credential's state.
pub fn key_state_word(state: KeyState) -> &'static str {
    match state {
        KeyState::NotNeeded => "no auth",
        KeyState::Saved => "key",
        KeyState::Disabled => "key not for chat",
        KeyState::Missing => "key missing",
    }
}

/// The input and output rates as the model lists show them: `$0.3/M in, $1.2/M out`.
pub fn cost_summary(cost: &Cost) -> String {
    format!("${}/M in, ${}/M out", cost.input, cost.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_read_per_million() {
        let c = Cost { input: 0.3, output: 1.2, ..Cost::default() };
        assert_eq!(cost_summary(&c), "$0.3/M in, $1.2/M out");
        assert_eq!(key_state_word(KeyState::Missing), "key missing");
    }
}
