//! Closed-loop tests against the mock keyer box and the radio it keys: the same
//! scripted field operator as `mock_radio_e2e.rs`, with the radio's headphone audio
//! running into the node in real time. One test per `keyer-` scenario in
//! [`hfnode::selftest::scenarios`].
//!
//! A test binary of their own, which cargo runs apart from the others: the mock
//! IC-7300's scenarios take all the processor they can get, and on a busy machine
//! (a CI runner) these, which must keep up with real time, would then miss it.
//! They run at most [`selftest::KEYER_MAX_SCALE`] times real time, or
//! `HFNODE_E2E_SCALE` if lower. A run the machine paused the test in for longer
//! than the scenario's timing allows is not judged and runs again (its `machine`
//! check, `selftest::run`).

use hfnode::selftest;

fn scale() -> f32 {
    std::env::var("HFNODE_E2E_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(selftest::DEFAULT_SCALE)
}

fn run(name: &str) {
    let s = selftest::scenario(name).unwrap_or_else(|| panic!("no scenario {name}"));
    let out = selftest::run(&s, scale());
    assert!(out.passed(), "\n{}", out.render());
    assert!(
        out.checks.iter().any(|c| c.name == "machine"),
        "no pause meter:\n{}",
        out.render()
    );
}

const NAMES: &[&str] = &[
    "keyer-tx",
    "keyer-rx-long",
    "keyer-agn",
    "keyer-cable-out",
    "keyer-stuck-key",
    "keyer-box-unplugged",
    "keyer-ht-tx",
    "keyer-ht-stuck-ptt",
];

#[test]
fn every_keyer_scenario_has_a_test() {
    let all: Vec<String> = selftest::scenarios()
        .into_iter()
        .filter(|s| s.radio.keyer)
        .map(|s| s.name)
        .collect();
    assert_eq!(all, NAMES);
}

#[test]
fn keyer_tx() {
    run("keyer-tx");
}

#[test]
fn keyer_rx_long() {
    run("keyer-rx-long");
}

#[test]
fn keyer_agn() {
    run("keyer-agn");
}

#[test]
fn keyer_cable_out() {
    run("keyer-cable-out");
}

#[test]
fn keyer_stuck_key() {
    run("keyer-stuck-key");
}

#[test]
fn keyer_box_unplugged() {
    run("keyer-box-unplugged");
}

#[test]
fn keyer_ht_tx() {
    run("keyer-ht-tx");
}

#[test]
fn keyer_ht_stuck_ptt() {
    run("keyer-ht-stuck-ptt");
}
