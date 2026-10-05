//! The keyer rig with the mock box and radio, under the station's safety layer.

use super::*;
use crate::keyer::mock::RadioSettings;
use crate::keyer::testbench::{bench, radio_secs, tx};
use crate::station::TxError;

#[test]
fn keys_a_transmission_the_radio_is_heard_sending() {
    let mut b = bench(|_| {});
    b.station.transmit(&tx(&["R 42 DE N0DE K"])).unwrap();
    let st = b.keyer_box.now();
    assert!(st.lines.iter().any(|l| l.contains("CW 20 R 42 DE N0DE K")));
    assert_eq!(st.ended(), Ended::Done);
    drop(st);
    assert!(!b.station.tx_inhibited());
    assert!(b.station.can_transmit());
}

#[test]
fn a_run_longer_than_the_link_timeout_is_kept_alive() {
    let mut b = bench(|_| {});
    // 29 characters at 20 wpm: about 14 s of keying, seven times the box's link
    // timeout.
    b.station
        .transmit(&tx(&["PARIS PARIS PARIS PARIS PARIS"]))
        .unwrap();
    let st = b.keyer_box.now();
    assert_eq!(st.ended(), Ended::Done);
    // A STATUS every 0.25 s, from the keep-alive alone.
    let polls = st.lines.iter().filter(|l| l.contains(" STATUS*")).count();
    assert!(polls > 30, "{polls} STATUS lines");
}

#[test]
fn a_radio_not_heard_keying_locks_out_until_the_next_window() {
    let mut b = bench(|r| r.cable_out = true);
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE K"])),
        Err(TxError::NotHeard)
    );
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE K"])),
        Err(TxError::SwrLockout)
    );
    assert_eq!(b.cw_lines(), 1);
    assert!(!b.station.tx_inhibited());
    // Cable back in: the next window start clears the lockout, keying nothing.
    b.radio.set(|r| r.cable_out = false);
    b.station.open_window().unwrap();
    assert_eq!(b.cw_lines(), 1);
    b.station.transmit(&tx(&["DE N0DE K"])).unwrap();
}

#[test]
fn faults_that_keep_the_sidetone_from_being_heard_lock_out() {
    type Fault = (&'static str, fn(&mut RadioSettings));
    let faults: [Fault; 2] = [
        ("sidetone off", |r| r.sidetone = 0.0),
        ("sidetone at another pitch", |r| r.pitch_hz = 900.0),
    ];
    for (name, f) in faults {
        let mut b = bench(f);
        assert_eq!(
            b.station.transmit(&tx(&["DE N0DE K"])),
            Err(TxError::NotHeard),
            "{name}"
        );
        assert!(!b.station.tx_inhibited(), "{name}");
    }
}

#[test]
fn a_key_stuck_at_the_radio_inhibits_transmitting() {
    let mut b = bench(|_| {});
    let at = b.now() + 1.0;
    b.radio.set(|r| r.stuck_from = Some(at));
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE N0DE K"])),
        Err(TxError::Inhibited)
    );
    assert!(b.station.tx_inhibited());
    let why = std::fs::read_to_string(b.inhibit_file()).unwrap();
    assert!(why.contains("not confirmed on receive"), "{why}");
}

#[test]
fn a_key_already_stuck_is_found_before_keying() {
    let mut b = bench(|r| r.stuck_from = Some(0.0));
    // Thirty seconds of steady sidetone with no keying from the node.
    thread::sleep(radio_secs(31.0));
    assert!(b.station.check().is_err());
    assert!(b.station.tx_inhibited());
    assert_eq!(b.cw_lines(), 0);
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE K"])),
        Err(TxError::Inhibited)
    );
}

#[test]
fn a_radio_switched_off_is_not_keyed() {
    let mut b = bench(|r| r.off = true);
    let e = b.station.transmit(&tx(&["DE N0DE K"])).unwrap_err();
    assert!(e.to_string().contains("dBFS"), "{e}");
    assert_eq!(b.cw_lines(), 0);
    assert!(!b.station.tx_inhibited());
}

#[test]
fn no_audio_from_the_radio_means_no_keying() {
    let mut b = bench(|r| r.unplugged = true);
    let e = b.station.transmit(&tx(&["DE N0DE K"])).unwrap_err();
    assert!(e.to_string().contains("no audio"), "{e}");
    assert_eq!(b.cw_lines(), 0);
    assert!(!b.station.tx_inhibited());
}

#[test]
fn a_box_unplugged_before_keying_keys_nothing_and_inhibits_nothing() {
    let mut b = bench(|_| {});
    b.keyer_box.unplug(true);
    assert!(b.station.transmit(&tx(&["DE N0DE K"])).is_err());
    assert_eq!(b.cw_lines(), 0);
    assert!(!b.station.tx_inhibited());
}

#[test]
fn a_box_unplugged_mid_run_fails_the_transmission_without_inhibiting() {
    let mut b = bench(|_| {});
    let kb = b.keyer_box.clone();
    let pull = thread::spawn(move || {
        thread::sleep(radio_secs(2.0));
        kb.unplug(true);
    });
    let r = b.station.transmit(&tx(&["PARIS PARIS PARIS PARIS PARIS"]));
    pull.join().unwrap();
    assert!(r.is_err());
    assert_ne!(r, Err(TxError::Inhibited));
    assert!(!b.station.tx_inhibited());
    // Plugged in again: it keys.
    b.keyer_box.unplug(false);
    thread::sleep(radio_secs(1.5));
    b.station.open_window().unwrap();
    b.station.transmit(&tx(&["DE N0DE K"])).unwrap();
}

#[test]
fn a_box_whose_control_loop_hangs_is_reset_by_its_watchdog() {
    let b = bench(|_| {});
    let rig = b.station.rig();
    let mut r = lock(&rig);
    r.send_cw("PARIS PARIS").unwrap();
    thread::sleep(radio_secs(0.3));
    r.test(&Command::TestHang).unwrap();
    // Hung with the key down at the next element at the latest; the watchdog
    // resets the box 0.5 s later and its key opens.
    thread::sleep(radio_secs(2.0));
    let st = b.keyer_box.now();
    assert_eq!(st.resets, 1);
    assert!(!st.key_down());
    let downs = st.downs(0, b.keyer_box.clock.ms());
    let longest = downs.iter().map(|&(a, b)| b - a).max().unwrap();
    assert!(
        longest <= u64::from(keyer_core::limits::WATCHDOG_MS) + 200,
        "{longest} ms"
    );
    drop(st);
    // The node learns the run ended early, and then, once the audio covers the
    // time the box went quiet, that the key is open.
    let first = r.is_transmitting();
    assert!(first.is_err(), "{first:?}");
    let deadline = Instant::now() + radio_secs(4.0);
    while r.is_transmitting().unwrap() {
        assert!(Instant::now() < deadline, "{:?}", r.transmit_detail());
        thread::sleep(radio_secs(0.1));
    }
}

#[test]
fn a_box_that_holds_its_key_trips_and_refuses_to_key_again() {
    let b = bench(|_| {});
    let rig = b.station.rig();
    let mut r = lock(&rig);
    r.send_cw("PARIS PARIS").unwrap();
    thread::sleep(radio_secs(0.2));
    r.test(&Command::TestStuck).unwrap();
    thread::sleep(radio_secs(2.5));
    let st = r.status().unwrap();
    assert_eq!(st.trip, Trip::Down);
    assert!(!st.busy());
    let longest = b
        .keyer_box
        .now()
        .downs(0, b.keyer_box.clock.ms())
        .iter()
        .map(|&(a, b)| b - a)
        .max()
        .unwrap();
    assert!(
        longest <= u64::from(keyer_core::limits::KEY_DOWN_MS) + 5,
        "{longest} ms"
    );
    let e = r.is_transmitting().unwrap_err();
    assert!(e.to_string().contains("tripped"), "{e}");
    let e = r.send_cw("TEST").unwrap_err();
    assert!(e.to_string().contains("TRIP"), "{e}");
}

#[test]
fn a_run_the_node_stops_is_not_taken_for_a_stuck_key() {
    let b = bench(|_| {});
    let rig = b.station.rig();
    let mut r = lock(&rig);
    r.send_cw("PARIS PARIS PARIS").unwrap();
    thread::sleep(radio_secs(1.0));
    r.stop_cw().unwrap();
    let deadline = Instant::now() + radio_secs(3.0);
    while r.is_transmitting().unwrap() {
        assert!(Instant::now() < deadline, "still transmitting");
        thread::sleep(radio_secs(0.1));
    }
}
