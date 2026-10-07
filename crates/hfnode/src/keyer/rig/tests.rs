//! The keyer rig with the mock box and radio, under the station's safety layer.

use super::*;
use crate::keyer::mock::RadioSettings;
use crate::keyer::testbench::{bench, bench_at, radio_secs, tx};
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
    // A STATUS every 0.25 s from the keep-alive: time-scaled, a busy test machine
    // sleeps longer than asked, so look for at least two per link timeout.
    let polls = st.lines.iter().filter(|l| l.contains(" STATUS*")).count();
    assert!(polls >= 14, "{polls} STATUS lines");
}

#[test]
fn the_link_test_sees_the_box_open_its_key_on_its_own() {
    // The one check that the node dying, or its cable coming out, leaves the radio
    // on receive: the node keys a run and then says nothing (the safety audit's
    // KB-11).
    let b = bench(|_| {});
    let rig = b.station.rig();
    let waited = lock(&rig).link_test("TTTT TTTT TTTT TTTT").unwrap();
    assert!(waited >= radio_secs(2.0), "{waited:?}");
    let st = b.keyer_box.now();
    assert_eq!(st.ended(), Ended::Link);
    // Its key opened on its own: nothing told it to stop after the run began.
    let stops = st
        .lines
        .iter()
        .skip_while(|l| !l.contains(" CW "))
        .filter(|l| l.contains(" STOP"))
        .count();
    assert_eq!(stops, 0, "{:?}", st.lines);
    drop(st);
    // And the run's keying still counts against the duty window.
    assert!(lock(&rig).rest_needed(Duration::from_secs(1)).unwrap() > Duration::ZERO);
}

#[test]
fn the_link_test_fails_on_a_box_that_keeps_keying() {
    // A box whose link timeout never fires: the node must say so, not pass.
    let b = bench(|_| {});
    b.keyer_box.now().deaf_to_silence = true;
    let rig = b.station.rig();
    let e = lock(&rig)
        .link_test("TTTT TTTT TTTT TTTT")
        .unwrap_err()
        .to_string();
    assert!(e.contains("link timeout did not open the key"), "{e}");
}

#[test]
fn a_run_still_keying_past_its_end_is_stopped_once() {
    // The box keys at half the speed it was asked (a wrong clock), so it is still
    // sending when the node's run is over. The keep-alive must stop it once and
    // fail the transmission, not keep answering STATUS for ever: with that arm
    // removed, every other test still passed (the safety audit's KB-10).
    let mut b = bench(|_| {});
    b.keyer_box.now().slow_wpm = Some(10);
    let e = b.station.transmit(&tx(&["PARIS PARIS"])).unwrap_err();
    assert_ne!(e, TxError::Inhibited, "{e}");
    // The keep-alive's STOP went out while the box was still keying, and the box
    // read the run as stopped: without it, STATUS would have kept the run alive
    // until the station's own stuck-key handling, long past its end.
    let st = b.keyer_box.now();
    let after_cw: Vec<&String> = st
        .lines
        .iter()
        .skip_while(|l| !l.contains(" CW "))
        .collect();
    let first_stop = after_cw
        .iter()
        .position(|l| l.contains(" STOP"))
        .unwrap_or_else(|| panic!("no STOP: {after_cw:?}"));
    // Before the station's own forced receive, which sends its own STOPs.
    assert!(first_stop < after_cw.len() - 1, "{after_cw:?}");
    assert_eq!(st.ended(), Ended::Stop);
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
fn audio_lost_mid_run_takes_the_key_as_held() {
    // The sound card's cable is pulled while the radio keys: the node cannot hear
    // its key open after the box's, so it takes it as held.
    let mut b = bench(|_| {});
    let settings = b.radio.settings.clone();
    let pull = thread::spawn(move || {
        thread::sleep(radio_secs(2.0));
        lock(&settings).unplugged = true;
    });
    let r = b.station.transmit(&tx(&["PARIS PARIS PARIS PARIS PARIS"]));
    pull.join().unwrap();
    assert_eq!(r, Err(TxError::Inhibited));
    assert!(b.station.tx_inhibited());
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

#[test]
fn a_key_closed_at_the_radio_just_before_a_piece_inhibits() {
    let mut b = bench(|_| {});
    let at = b.now();
    b.radio.set(|r| r.stuck_from = Some(at));
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE K"])),
        Err(TxError::Inhibited)
    );
    let why = std::fs::read_to_string(b.inhibit_file()).unwrap();
    assert!(why.contains("after the box opened its key"), "{why}");
}

#[test]
fn a_hold_at_the_radio_that_clears_by_itself_still_inhibits() {
    // The key stays closed at the radio when the box opens its own, and lets go a
    // few seconds later: soon enough that the station would see receive by its own
    // deadline and count the transmission done. The radio keyed on its own, so the
    // node must stop for good and say why (the safety audit's KB-2(i), which found
    // 2 s and 4 s holds passing as Ok(())).
    for hold in [2.0, 4.0] {
        let mut b = bench(|_| {});
        let kb = b.keyer_box.clone();
        let radio = &b.radio;
        let station = &mut b.station;
        let sent = thread::scope(|sc| {
            // The hold starts the moment the box's run is over, so the run itself is
            // keyed and heard as it should be.
            sc.spawn(move || {
                let deadline = Instant::now() + radio_secs(30.0);
                while Instant::now() < deadline {
                    if kb.now().ended() == Ended::Done {
                        let at = kb.clock.secs();
                        radio.set(|r| {
                            r.stuck_from = Some(at);
                            r.stuck_until = Some(at + hold);
                        });
                        return;
                    }
                    thread::sleep(radio_secs(0.02));
                }
                panic!("the box never finished a run");
            });
            station.transmit(&tx(&["DE N0DE K"]))
        });
        assert_eq!(sent, Err(TxError::Inhibited), "a {hold} s hold");
        assert!(b.station.tx_inhibited(), "a {hold} s hold");
        let why = std::fs::read_to_string(b.inhibit_file()).unwrap();
        assert!(
            why.contains("key is closed at the radio"),
            "{why}: a {hold} s hold"
        );
    }
}

#[test]
fn a_key_held_at_the_radio_is_never_forgotten() {
    // The key stays closed at the radio when the box opens its own, then lets go.
    // Before, the rig called that "still transmitting" and went on keying once the
    // audio came back (the safety audit's KB-2(i)); now the radio has keyed on its
    // own once, which is enough to stop for good.
    let b = bench(|_| {});
    let rig = b.station.rig();
    let mut r = lock(&rig);
    r.send_cw("DE N0DE K").unwrap();
    let deadline = Instant::now() + radio_secs(30.0);
    while b.keyer_box.now().ended() != Ended::Done {
        assert!(Instant::now() < deadline, "the box never finished the run");
        thread::sleep(radio_secs(0.02));
    }
    let at = b.keyer_box.clock.secs();
    b.radio.set(|s| {
        s.stuck_from = Some(at);
        s.stuck_until = Some(at + 2.0);
    });
    // The audio shows the radio's key held, with the box's open ...
    let why = loop {
        match r.is_transmitting() {
            Err(e) => break e.to_string(),
            Ok(_) => {
                assert!(Instant::now() < deadline, "the held key was never seen");
                thread::sleep(radio_secs(0.1));
            }
        }
    };
    assert!(why.contains("key is closed at the radio"), "{why}");
    // ... and it stays refused after the key lets go.
    thread::sleep(radio_secs(4.0));
    let e = r.is_transmitting().unwrap_err().to_string();
    assert!(e.contains("key is closed at the radio"), "{e}");
    assert!(r.held_key().is_some());
    assert!(r.refusal().is_some());
    assert!(r.send_cw("DE N0DE K").is_err());
    assert_eq!(b.cw_lines(), 1);
}

#[test]
fn a_key_closed_at_the_radio_is_not_keyed_over_and_inhibits_while_idle() {
    let mut b = bench(|_| {});
    let at = b.now();
    b.radio.set(|r| r.stuck_from = Some(at));
    thread::sleep(radio_secs(3.0));
    let e = b.station.transmit(&tx(&["DE N0DE K"])).unwrap_err();
    assert!(e.to_string().contains("steady tone"), "{e}");
    assert_eq!(b.cw_lines(), 0);
    // Not keyed over, and not taken for a fault of the node's keying...
    assert!(b.station.can_transmit());
    // ... but once the tone has gone on for 30 s the station inhibits by itself,
    // with nothing keyed and no check due. Its forced receive first waits for the
    // audio in real time, whatever the scale.
    let deadline = Instant::now() + radio_secs(40.0) + Duration::from_secs(2);
    while !b.station.tx_inhibited() {
        assert!(Instant::now() < deadline, "not inhibited");
        thread::sleep(radio_secs(0.5));
    }
    let why = std::fs::read_to_string(b.inhibit_file()).unwrap();
    assert!(why.contains("steady tone"), "{why}");
}

#[test]
fn a_box_that_trips_inhibits_transmitting() {
    let mut b = bench(|_| {});
    {
        let rig = b.station.rig();
        let mut r = lock(&rig);
        r.send_cw("PARIS PARIS").unwrap();
        thread::sleep(radio_secs(0.2));
        r.test(&Command::TestStuck).unwrap();
    }
    thread::sleep(radio_secs(2.5));
    assert_eq!(
        b.station.transmit(&tx(&["DE N0DE K"])),
        Err(TxError::Inhibited)
    );
    let why = std::fs::read_to_string(b.inhibit_file()).unwrap();
    assert!(why.contains("tripped"), "{why}");
}

#[test]
fn a_run_stopped_at_full_speed_is_confirmed_on_receive_in_real_time() {
    // At real time: the station's waits between its attempts are wall-clock, and
    // the audio must catch up with the stop before the key shows open.
    let b = bench_at(1.0, |_| {});
    let rig = b.station.rig();
    let mut r = lock(&rig);
    r.set_key_speed(48).unwrap();
    r.send_cw("PARIS PARIS PARIS").unwrap();
    thread::sleep(Duration::from_millis(400));
    crate::station::force_receive(&mut *r).unwrap();
    assert!(!r.is_transmitting().unwrap());
}
