use super::*;
use crate::keyer::mock::RadioSettings;
use crate::keyer::testbench::{bench, handheld, radio_secs, tx, SCALE};

#[test]
fn check_passes_on_a_working_station_and_keys_nothing() {
    let b = bench(|_| {});
    let rig = b.station.rig();
    let (report, ok) = check(&mut lock(&rig), -65.0);
    assert!(ok, "{report}");
    assert!(report.contains("PICO2-KEYER"), "{report}");
    assert!(report.contains("ok"), "{report}");
    assert_eq!(b.cw_lines(), 0);
}

#[test]
fn check_fails_with_the_radio_off() {
    let b = bench(|r| r.off = true);
    let rig = b.station.rig();
    let (report, ok) = check(&mut lock(&rig), -65.0);
    assert!(!ok, "{report}");
    assert!(report.contains("turn the radio's volume up"), "{report}");
}

#[test]
fn key_reports_the_run_heard() {
    let mut b = bench(|_| {});
    let j = key(&mut b.station, "DE N0DE").unwrap().unwrap();
    assert!(j.heard, "{j}");
}

#[test]
fn sidetone_measures_the_delay_and_the_pitch() {
    let mut b = bench(|r| r.pitch_hz = 650.0);
    let rep = sidetone(&mut b.station, "DE N0DE").unwrap();
    let p = rep.pitch_hz.unwrap();
    assert!((p - 650.0).abs() <= 5.0, "{p}");
    let (text, ok) = rep.explain(600.0);
    assert!(!ok, "{text}");
    assert!(text.contains("keyer.sidetone_hz = 650"), "{text}");
    let (text, ok) = rep.explain(650.0);
    assert!(ok, "{text}");
}

#[test]
fn a_sidetone_too_close_to_the_band_fails_the_check() {
    // The node takes a tone within 10 dB of the measured sidetone for the sidetone,
    // so a sidetone only 6 or 9 dB over the band noise cannot tell a key held at
    // the radio from the band (the safety audit's KB-2(ii)); nor 13, short of the
    // 15 dB the check asks for, which a gate at 10 dB would pass.
    for margin in [6.0f32, 9.0, 13.0] {
        let mut b = bench(move |r| {
            // Band noise is a standard deviation, the sidetone an amplitude: a sine
            // of amplitude a has the power of noise of standard deviation a/sqrt 2.
            r.noise = r.sidetone / 2.0f32.sqrt() / 10.0f32.powf(margin / 20.0);
        });
        let rep = sidetone(&mut b.station, "DE N0DE").unwrap();
        let (text, ok) = rep.explain(600.0);
        assert!(!ok, "a {margin} dB margin passed: {text}");
        assert!(text.contains("dB over it, under the 15 dB"), "{text}");
    }
}

#[test]
fn a_test_that_never_hears_the_key_down_does_not_pass() {
    // The identification is heard, then the key cable comes out: the box still
    // hangs and its watchdog still resets it, but the audio shows no key-down, so
    // the test measured nothing and must not pass (the safety audit's KB-3, which
    // saw "longest: 0ns ... passed: true").
    let mut b = bench(|_| {});
    let kb = b.keyer_box.clone();
    let radio = &b.radio;
    let station = &mut b.station;
    let rep = thread::scope(|sc| {
        sc.spawn(move || {
            // Once the identification is keyed and over, the cable comes out.
            let deadline = Instant::now() + radio_secs(30.0);
            while Instant::now() < deadline {
                if kb.now().lines.iter().any(|l| l.contains("CW 20 DE N0DE"))
                    && kb.now().ended() == keyer_core::keyer::Ended::Done
                {
                    radio.set(|r| r.cable_out = true);
                    return;
                }
                thread::sleep(radio_secs(0.02));
            }
        });
        hangtest(station, "DE N0DE", SCALE)
    });
    let rep = rep.expect("the test itself ran");
    assert!(!rep.passed, "{rep:?}");
    assert!(
        rep.notes.iter().any(|n| n.contains("nothing measured")),
        "{rep:?}"
    );
}

#[test]
fn a_handheld_is_never_told_to_run_the_sidetone_check() {
    // `hfnode keyer sidetone` refuses a handheld, so a box test's failure on one
    // must point to what it can run: `check`, and `key "TEST"`, which reports the
    // receive noise going quiet. The key line keeps the sidetone check.
    for (output, wants, never) in [
        (Output::Ptt, "keyer key \"TEST\"", "keyer sidetone"),
        (Output::Key, "keyer sidetone", "keyer key"),
    ] {
        let mut notes = Vec::new();
        assert!(!check_measured(
            Duration::ZERO,
            Duration::ZERO,
            output,
            &mut notes
        ));
        let first = not_heard_first(output, "not heard");
        for text in [&notes[0], &first] {
            assert!(text.contains(wants), "{output:?}: {text}");
            assert!(!text.contains(never), "{output:?}: {text}");
        }
    }
}

#[test]
fn hangtest_sees_the_watchdog_open_the_key() {
    let mut b = bench(|_| {});
    let rep = hangtest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(rep.passed, "{rep:?}");
    assert!(rep.longest >= Duration::from_millis(300), "{rep:?}");
    assert_eq!(b.keyer_box.now().resets, 1);
    // It identified first, before holding the key down.
    assert!(b.keyer_box.now().lines.last().is_some());
    assert!(b
        .keyer_box
        .now()
        .lines
        .iter()
        .any(|l| l.contains("CW 20 DE N0DE")));
    // It came back tripped, so nothing more is keyed, and the node's stop latches
    // its inhibit, as after `stucktest`.
    assert_eq!(b.keyer_box.now().trip(), Trip::Watchdog);
    assert!(key(&mut b.station, "DE N0DE").is_err());
    let file = b.inhibit_file();
    drop(b.station);
    let why = std::fs::read_to_string(file).unwrap();
    assert!(why.contains("watchdog"), "{why}");
}

#[test]
fn stucktest_sees_the_key_down_limit_trip_the_box() {
    let mut b = bench(|_| {});
    let rep = stucktest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(rep.passed, "{rep:?}");
    assert!(rep.longest >= Duration::from_millis(800), "{rep:?}");
    // Tripped: nothing more is keyed.
    thread::sleep(radio_secs(1.0));
    assert!(key(&mut b.station, "DE N0DE").is_err());
}

#[test]
fn a_box_that_comes_back_from_its_watchdog_untripped_fails_the_hang_test() {
    // It restarted and opened its key, but reports no trip: after its watchdog
    // fires it must key nothing until it is plugged in again.
    let mut b = bench(|_| {});
    b.keyer_box.now().hides_watchdog_trip = true;
    let rep = hangtest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(!rep.passed, "{rep:?}");
    assert_eq!(b.keyer_box.now().resets, 1);
    assert!(
        rep.notes.iter().any(|n| n.contains("not tripped WATCHDOG")),
        "{rep:?}"
    );
}

#[test]
fn a_box_that_never_resets_fails_the_hang_test() {
    // A radio whose key stays closed after the hang: as a box with no watchdog
    // would leave it.
    let mut b = bench(|_| {});
    let at = b.now() + 0.5;
    b.radio.set(|r| r.stuck_from = Some(at));
    assert!(hangtest(&mut b.station, "DE N0DE", SCALE).map_or(true, |r| !r.passed));
}

#[test]
fn linktest_identifies_once_the_radio_is_back_on_receive() {
    // `hfnode keyer linktest` as a whole, on a radio's key line and on a handheld's
    // PTT: the box opens its key by itself, and the node then identifies, since the
    // test text carries no call. It used to identify at once, before the audio
    // showed the radio back on receive: the station refused that as "on transmit
    // without the node keying it", the command failed after printing `passed`, the
    // test went out with no call sign, and on the key line the inhibit latched.
    for (what, mut b) in [("key line", bench(|_| {})), ("handheld", handheld(|_| {}))] {
        let dir = b.dir.path().to_path_buf();
        let waited =
            linktest(&mut b.station, "DE N0DE", &dir).unwrap_or_else(|e| panic!("{what}: {e:#}"));
        assert!(waited >= radio_secs(2.0), "{what}: {waited:?}");
        let st = b.keyer_box.now();
        let runs: Vec<&String> = st
            .lines
            .iter()
            .filter(|l| l.contains(" CW ") || l.contains(" MCW "))
            .collect();
        assert_eq!(runs.len(), 2, "{what}: {runs:?}");
        assert!(runs[0].contains(LINK_TEST_TEXT), "{what}: {runs:?}");
        assert!(runs[1].contains("DE N0DE"), "{what}: {runs:?}");
        assert_eq!(
            st.ended(),
            keyer_core::keyer::Ended::Done,
            "{what}: the ID ran to its end"
        );
        drop(st);
        assert!(!b.inhibit_file().exists(), "{what}: the inhibit latched");
        assert!(b.station.can_transmit(), "{what}");
    }
}

#[test]
fn rx_confirms_the_key_open_and_latches_nothing() {
    let b = bench(|_| {});
    let dir = tempfile::tempdir().unwrap();
    rx(
        &mut lock(&b.station.rig()),
        dir.path(),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(!InhibitLatch::in_dir(dir.path()).is_set());
}

#[test]
fn rx_latches_the_inhibit_when_the_key_is_not_confirmed_open() {
    // Each way `keyer rx` can fail to confirm the key open stops the node keying
    // after it, until someone has looked at the radio (the safety audit's KB-2, in
    // its review of PR #13). `rx` gets a state directory of its own, so that the
    // inhibit found there is the one it latched.
    type Fault = (&'static str, &'static str, fn(&mut RadioSettings));
    let faults: [Fault; 3] = [
        ("no audio", "no audio from the radio", |r| {
            r.unplugged = true
        }),
        (
            "a key closed at the radio",
            "a steady tone at the sidetone pitch",
            |r| r.stuck_from = Some(0.0),
        ),
        (
            "a key held at the radio after a run",
            "not confirmed on receive",
            |_| {},
        ),
    ];
    for (name, why, fault) in faults {
        let mut b = bench(fault);
        if name.ends_with("after a run") {
            let at = b.now() + 1.0;
            b.radio.set(|r| r.stuck_from = Some(at));
            assert!(b.station.transmit(&tx(&["DE N0DE N0DE K"])).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let e = rx(
            &mut lock(&b.station.rig()),
            dir.path(),
            Duration::from_secs(1),
        )
        .expect_err(name);
        assert!(
            format!("{e:#}").contains("not confirmed open"),
            "{name}: {e:#}"
        );
        let file = dir.path().join(crate::station::INHIBIT_FILE);
        let latched =
            std::fs::read_to_string(&file).unwrap_or_else(|_| panic!("{name}: no inhibit latched"));
        assert!(latched.contains(why), "{name}: {latched}");
    }
}

// An FM handheld on the box's PTT.

#[test]
fn check_on_a_handheld_reports_the_ptt_line() {
    let b = handheld(|_| {});
    let rig = b.station.rig();
    let (report, ok) = check(&mut lock(&rig), -65.0);
    assert!(ok, "{report}");
    assert!(report.contains("PTT limit 60 s"), "{report}");
    assert!(report.contains("PTT line high"), "{report}");
    assert_eq!(b.cw_lines(), 0);
}

#[test]
fn check_fails_with_the_handheld_off() {
    let b = handheld(|r| r.off = true);
    let rig = b.station.rig();
    let (report, ok) = check(&mut lock(&rig), -65.0);
    assert!(!ok, "{report}");
    assert!(report.contains("PTT line LOW"), "{report}");
    assert!(report.contains("squelch must be open"), "{report}");
}

#[test]
fn key_reports_a_handheld_keyed() {
    let mut b = handheld(|_| {});
    let j = key(&mut b.station, "DE N0DE").unwrap().unwrap();
    assert!(j.heard, "{j}");
    assert!(j.to_string().starts_with("keyed"), "{j}");
}

#[test]
fn hangtest_on_a_handheld_sees_the_watchdog_release_the_ptt() {
    let mut b = handheld(|_| {});
    let rep = hangtest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(rep.passed, "{rep:?}");
    // The lead, then the watchdog's 0.5 s from the first element.
    assert!(rep.longest >= Duration::from_millis(800), "{rep:?}");
    let st = b.keyer_box.now();
    assert_eq!(st.resets, 1);
    assert!(st.lines.iter().any(|l| l.contains("MCW 20 DE N0DE")));
}

#[test]
fn stucktest_on_a_handheld_sees_the_key_down_limit_trip_the_box() {
    let mut b = handheld(|_| {});
    let rep = stucktest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(rep.passed, "{rep:?}");
    // The lead, then the tone held for the 1 s limit.
    assert!(rep.longest >= Duration::from_millis(1300), "{rep:?}");
    assert_eq!(b.keyer_box.now().trip(), Trip::Down);
}
