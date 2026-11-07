//! What a custom endpoint's models cost: the `cost` rates in `providers.toml` are the only price
//! such a model has, and turns without one wait for a table that names them. One test, because every
//! test in a binary shares one profile home and the rates are read from that file.

use mu_base::message::Usage;
use mu_provider::model::cost::CostLedger;
use mu_provider::model::pricing;

fn usage() -> Usage {
    Usage { input: 1_000_000, output: 200_000, cache_read: 300_000, cache_write: 100_000, ..Default::default() }
}

/// The profile's `providers.toml`, in this test binary's own `MU_HOME`.
fn write_definitions(text: &str) {
    let home = mu_testutil::home();
    std::fs::write(home.join("providers.toml"), text).unwrap();
}

fn definitions(other: &str) -> String {
    format!(
        r#"
[providers.costly]
base_url = "https://endpoint.example/v1"

[providers.costly.models."org/a".cost]
input = 2.0
output = 8.0
cache_read = 0.5

[providers.local]
base_url = "http://127.0.0.1:8000/v1"
auth = "none"

[providers.local.models."org/free".cost]
input = 0.0

{other}"#
    )
}

#[test]
fn configured_rates_price_a_custom_models_turns() {
    write_definitions(&definitions(""));

    // The rates are dollars per million tokens, as the catalogue's are.
    let c = pricing::cost("costly/org/a", &usage()).expect("the configured rates price it");
    assert_eq!((c.input, c.output, c.cache_read, c.cache_write), (2.0, 1.6, 0.15, 0.0));
    assert_eq!(c.total, 3.75);

    // A turn is recorded as mu records it: the provider, and the full `provider/model` id.
    let mut turn = usage();
    let mut ledger = CostLedger::default();
    ledger.record("costly", "costly/org/a", &mut turn);
    assert_eq!(turn.cost.total, 3.75, "the turn is stored with its price");
    assert_eq!(ledger.total, 3.75);
    assert!(!ledger.settle(), "nothing waits for a price");
    assert_eq!((ledger.tokens_in, ledger.tokens_out), (1_400_000, 200_000));

    // A model with no rates is unpriced, not free: its usage waits for a table that names it.
    let mut turn = usage();
    let mut ledger = CostLedger::default();
    ledger.record("costly", "costly/org/other", &mut turn);
    assert_eq!((ledger.total, turn.cost.total), (0.0, 0.0), "no rates, no dollars");
    assert!(ledger.settle(), "usage waits for a price");

    // The file is read afresh, so rates added for it price what the session already spent.
    write_definitions(&definitions("[providers.costly.models.\"org/other\".cost]\ninput = 1.0\noutput = 4.0\n"));
    assert!(!ledger.settle(), "the table prices it now");
    assert_eq!(ledger.total, 1.0 + 0.8);

    // A configured zero is a price too: it settles, rather than waiting for one.
    let free = pricing::cost("local/org/free", &usage()).expect("a configured zero is still a price");
    assert_eq!(free.total, 0.0);
    let mut turn = usage();
    let mut ledger = CostLedger::default();
    ledger.record("local", "local/org/free", &mut turn);
    assert!(!ledger.settle(), "a zero rate is not a missing price");
}
