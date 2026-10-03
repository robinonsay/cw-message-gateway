//! Closed-loop tests against the byte-level mock IC-7300: a scripted field operator
//! keys CW audio into the whole node, listens to what the mock radio keys and
//! reacts to it. One test per scenario in [`hfnode::selftest::scenarios`].
//!
//! The scenarios run time-scaled ([`selftest::DEFAULT_SCALE`] times real time).
//! On a slow machine, such as a Raspberry Pi, set `HFNODE_E2E_SCALE` lower, for
//! example `HFNODE_E2E_SCALE=20 cargo test --test mock_radio_e2e`.

use hfnode::selftest::{self, Outcome};

fn scale() -> f32 {
    std::env::var("HFNODE_E2E_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(selftest::DEFAULT_SCALE)
}

fn run(name: &str) -> Outcome {
    let s = selftest::scenario(name).unwrap_or_else(|| panic!("no scenario {name}"));
    let out = selftest::run(&s, scale());
    assert!(out.passed(), "\n{}", out.render());
    out
}

/// The scenario tests, by name; `every_scenario_has_a_test` keeps this list whole.
const NAMES: &[&str] = &[
    "tx",
    "tx-gateway-down",
    "rx-empty",
    "rx-one",
    "rx-several",
    "rx-long",
    "wx-home",
    "wx-grid",
    "no-abort",
    "agn",
    "lost-read-back",
    "replayed-code",
    "wrong-code",
    "garbled-callsign",
    "trailing-noise",
    "over-word-mid-message",
    "speed-10wpm",
    "speed-15wpm",
    "speed-20wpm",
    "speed-25wpm",
    "speed-30wpm",
    "snr-20db",
    "snr-6db",
    "snr-3db",
    "snr-0db",
    "hand-keyed",
    "sidetone",
    "echo-off",
    "fault-high-swr",
    "fault-foldback",
    "fault-stuck-tx",
    "fault-stuck-key",
    "fault-jammed-tx",
    "fault-civ-ng",
    "fault-civ-lost-reply",
    "fault-civ-late-reply",
    "fault-tune-hang",
];

macro_rules! scenario_tests {
    ($($test:ident => $name:literal,)*) => {
        $(
            #[test]
            fn $test() {
                run($name);
            }
        )*

        #[test]
        fn every_scenario_has_a_test() {
            let tested = [$($name),*];
            assert_eq!(tested, NAMES);
            let all: Vec<String> = selftest::scenarios().into_iter().map(|s| s.name).collect();
            assert_eq!(all, NAMES);
        }
    };
}

scenario_tests! {
    tx => "tx",
    tx_gateway_down => "tx-gateway-down",
    rx_empty => "rx-empty",
    rx_one => "rx-one",
    rx_several => "rx-several",
    rx_long => "rx-long",
    wx_home => "wx-home",
    wx_grid => "wx-grid",
    no_abort => "no-abort",
    agn => "agn",
    lost_read_back => "lost-read-back",
    replayed_code => "replayed-code",
    wrong_code => "wrong-code",
    garbled_callsign => "garbled-callsign",
    trailing_noise => "trailing-noise",
    over_word_mid_message => "over-word-mid-message",
    speed_10wpm => "speed-10wpm",
    speed_15wpm => "speed-15wpm",
    speed_20wpm => "speed-20wpm",
    speed_25wpm => "speed-25wpm",
    speed_30wpm => "speed-30wpm",
    snr_20db => "snr-20db",
    snr_6db => "snr-6db",
    snr_3db => "snr-3db",
    snr_0db => "snr-0db",
    hand_keyed => "hand-keyed",
    sidetone => "sidetone",
    echo_off => "echo-off",
    fault_high_swr => "fault-high-swr",
    fault_foldback => "fault-foldback",
    fault_stuck_tx => "fault-stuck-tx",
    fault_stuck_key => "fault-stuck-key",
    fault_jammed_tx => "fault-jammed-tx",
    fault_civ_ng => "fault-civ-ng",
    fault_civ_lost_reply => "fault-civ-lost-reply",
    fault_civ_late_reply => "fault-civ-late-reply",
    fault_tune_hang => "fault-tune-hang",
}

/// The clean test vectors decode to exactly the text the manifest gives for them,
/// and the noisy ones are written alongside.
#[test]
fn test_vectors_decode_to_their_manifest_text() {
    let dir = tempfile::tempdir().unwrap();
    let files = selftest::write_vectors(
        dir.path(),
        &[12.0, 18.0, 25.0],
        &[None, Some(10.0)],
        0.03,
        600.0,
    )
    .unwrap();
    assert_eq!(files.len(), 3 * 2 * selftest::vectors().len());
    let manifest = std::fs::read_to_string(dir.path().join("manifest.txt")).unwrap();
    assert!(manifest.contains("TEST ONLY"));
    assert_eq!(
        std::fs::read(dir.path().join("test-only.key")).unwrap(),
        selftest::TEST_KEY
    );
    let rows: Vec<Vec<&str>> = manifest
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split('\t').collect())
        .collect();
    assert_eq!(rows.len(), files.len());
    for cols in rows.iter().filter(|c| c[2] == "-") {
        let (file, text) = (cols[0], cols[3]);
        let (samples, rate) = hfnode::audio::read_wav(&dir.path().join(file)).unwrap();
        let mut d = cw::Decoder::new(cw::DecoderConfig::new(rate, 600.0));
        let mut ev = d.push(&samples);
        ev.extend(d.flush());
        assert_eq!(cw::events_to_text(&ev).trim(), text, "{file}");
    }
}
