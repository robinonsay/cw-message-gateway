use super::*;
use crate::keyer::testbench::{bench, radio_secs, SCALE};

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
fn hangtest_sees_the_watchdog_open_the_key() {
    let mut b = bench(|_| {});
    let rep = hangtest(&mut b.station, "DE N0DE", SCALE).unwrap();
    assert!(rep.passed, "{rep:?}");
    assert!(rep.longest >= Duration::from_millis(300), "{rep:?}");
    assert_eq!(b.keyer_box.now().resets, 1);
    // It identified afterwards.
    assert!(b.keyer_box.now().lines.last().is_some());
    assert!(b
        .keyer_box
        .now()
        .lines
        .iter()
        .any(|l| l.contains("CW 20 DE N0DE")));
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
fn a_box_that_never_resets_fails_the_hang_test() {
    // A radio whose key stays closed after the hang: as a box with no watchdog
    // would leave it.
    let mut b = bench(|_| {});
    let at = b.now() + 0.5;
    b.radio.set(|r| r.stuck_from = Some(at));
    assert!(hangtest(&mut b.station, "DE N0DE", SCALE).map_or(true, |r| !r.passed));
}
