mod common;

#[test]
fn the_harness_starts_an_agent() {
    let _mu = need_mu!();
    let base = common::serve(vec![common::sse(common::ANSWER)]);
    let home = common::home_with_key(&base);
    let cwd = common::tmp();
    let agent = common::Agent::start(&home, &cwd, &[]);
    assert!(!agent.session.is_empty());
}
