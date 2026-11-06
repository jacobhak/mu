use mu_base::message::Usage;
use mu_provider::auth::chatgpt;
use mu_provider::model::cost::CostLedger;
use mu_provider::model::pricing;

fn usage() -> Usage {
    Usage { input: 1_000_000, output: 200_000, cache_read: 300_000, cache_write: 100_000, ..Default::default() }
}

#[test]
fn chatgpt_plan_prices_live_usage_like_the_gateway_model() {
    let expected = pricing::cost("openai/gpt-5.5", &usage()).expect("the OpenAI model has a price");
    assert!(expected.total > 0.0);

    for model in ["gpt-5.5", "openai-chatgpt/gpt-5.5"] {
        let mut ledger = CostLedger::default();
        let mut turn = usage();
        ledger.record(chatgpt::PROVIDER_ID, model, &mut turn);
        assert_eq!(turn.cost.input, expected.input);
        assert_eq!(turn.cost.output, expected.output);
        assert_eq!(turn.cost.cache_read, expected.cache_read);
        assert_eq!(turn.cost.cache_write, expected.cache_write);
        assert_eq!(turn.cost.total, expected.total);
        assert_eq!(ledger.total, expected.total);
        assert_eq!((ledger.tokens_in, ledger.tokens_out), (1_400_000, 200_000));
    }
}

#[test]
fn resumed_chatgpt_usage_restores_stored_and_previously_unpriced_costs() {
    let expected = pricing::cost("openai/gpt-5.5", &usage()).expect("the OpenAI model has a price");
    assert!(expected.total > 0.0);
    let mut recorded = usage();
    let mut live = CostLedger::default();
    live.record(chatgpt::PROVIDER_ID, "openai-chatgpt/gpt-5.5", &mut recorded);

    let mut resumed = CostLedger::default();
    resumed.add_stored(chatgpt::PROVIDER_ID, "openai-chatgpt/gpt-5.5", &recorded);
    resumed.add_stored(chatgpt::PROVIDER_ID, "gpt-5.5", &usage());
    assert!(!resumed.settle());
    assert_eq!(resumed.total, expected.total * 2.0);
}

#[test]
fn unknown_chatgpt_models_wait_for_prices_like_gateway_models() {
    let mut ledger = CostLedger::default();
    let mut turn = usage();
    ledger.record(chatgpt::PROVIDER_ID, "unknown-regression-model", &mut turn);
    assert_eq!(ledger.total, 0.0);
    assert_eq!((ledger.tokens_in, ledger.tokens_out), (1_400_000, 200_000));
    assert!(ledger.settle(), "unpriced usage must remain pending for a catalogue refresh");
}
