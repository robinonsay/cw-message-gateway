use super::mock::{MockFirmware, Off};
use super::*;
use crate::session::Transmission;
use crate::station::{
    duty_for_power, po_limit, Station, StationConfig, TxError, DUTY_WINDOW, ID_INTERVAL,
    MAX_TRANSMISSION,
};
use crate::storm::StormHold;
use std::io::Write as _;

/// Morse goes this many times faster than real time, in the firmware and the rig.
const SCALE: f32 = 50.0;
const FREQ: u64 = 144_060_000;
/// 29 characters, 243 units: about 0.29 s at 20 wpm here.
const LONG: &str = "PARIS PARIS PARIS PARIS PARIS";

fn settings() -> Settings {
    Settings {
        power: Power::Low,
        duty: 1.0,
        duty_window: Duration::from_secs(60),
        busy_quiet: Duration::ZERO,
        busy_max_wait: Duration::from_millis(500),
        run_slack: Duration::from_millis(100),
        max_run: Duration::from_secs(2),
        reply_timeout: Duration::from_millis(50),
        time_scale: SCALE,
    }
}

/// Firmware with the shortest link timeout the node accepts, 1 s.
fn firmware(scale: f32) -> MockFirmware {
    let fw = MockFirmware::new(scale);
    fw.set_hello(1, Duration::from_secs(60), MIN_LINK_TIMEOUT);
    fw
}

fn open(fw: &MockFirmware, set: Settings) -> Result<Handheld> {
    let link = Link::new(Box::new(fw.clone()), set.reply_timeout);
    Handheld::new(link, set, 20)
}

/// Opened and in CW mode.
fn ready_with(set: Settings) -> (Handheld, MockFirmware) {
    let fw = firmware(set.time_scale);
    let mut h = open(&fw, set).unwrap();
    h.set_mode_cw().unwrap();
    (h, fw)
}

fn ready() -> (Handheld, MockFirmware) {
    ready_with(settings())
}

fn wait_receive(h: &mut Handheld, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !h.is_transmitting().unwrap() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn off(fw: &MockFirmware, run: usize) -> (Duration, Off) {
    let r = &fw.runs()[run];
    let (at, why) = r.off.expect("run ended");
    (at - r.on, why)
}

#[test]
fn opening_stops_the_firmware_and_confirms_receive() {
    let fw = firmware(SCALE);
    fw.key_by_hand();
    let h = open(&fw, settings()).unwrap();
    assert_eq!(off(&fw, 0).1, Off::Stop);
    let bodies: Vec<String> = fw.received().into_iter().map(|r| r.1).collect();
    assert_eq!(bodies, ["HELLO", "STOP", "STATUS"]);
    assert!(h.describe().contains("MOCK-CW"), "{}", h.describe());
}

#[test]
fn firmware_without_limits_of_its_own_is_refused() {
    // One thing wrong at a time.
    for (version, tx_limit, link, why) in [
        (2, 60_000, 1000, "version 2"),
        (1, 0, 1000, "transmit limit is 0 s"),
        (1, 61_000, 1000, "transmit limit is 61 s"),
        (1, 60_000, 500, "link timeout is 500 ms"),
        (1, 60_000, 5000, "link timeout is 5000 ms"),
    ] {
        let fw = firmware(SCALE);
        fw.set_hello(
            version,
            Duration::from_millis(tx_limit),
            Duration::from_millis(link),
        );
        let e = open(&fw, settings()).err().unwrap().to_string();
        assert!(e.contains(why), "{e}");
    }
    let fw = firmware(SCALE);
    fw.set_hello(1, Duration::from_secs(60), Duration::from_millis(1000));
    assert!(open(&fw, settings()).is_ok());
    let fw = firmware(SCALE);
    fw.set_silent(true);
    let e = open(&fw, settings()).err().unwrap();
    assert!(
        format!("{e:#}").contains("no hfnode CW firmware answering"),
        "{e:#}"
    );
    let fw = firmware(SCALE);
    fw.key_by_hand();
    fw.set_ignore_stop(true);
    let e = open(&fw, settings()).err().unwrap();
    assert!(e.to_string().contains("still reads transmitting"), "{e}");
}

#[test]
fn a_run_is_kept_alive_and_read_back_as_ended() {
    // Slow enough (1.46 s) to outlast the 1 s link timeout.
    let mut set = settings();
    set.time_scale = 10.0;
    let (mut h, fw) = ready_with(set);
    h.send_cw(LONG).unwrap();
    assert!(h.is_transmitting().unwrap());
    // Only the keep-alives talk to the firmware meanwhile.
    thread::sleep(Duration::from_millis(1200));
    assert!(wait_receive(&mut h, Duration::from_secs(3)));
    let (lasted, why) = off(&fw, 0);
    assert_eq!(why, Off::Done, "not ended by the link timeout");
    assert!(lasted > Duration::from_millis(1400), "{lasted:?}");
    // Counted on the air for the duty cycle.
    let st = lock(&h.shared.state);
    assert!(!st.keyed);
    assert_eq!(st.on_air.len(), 1);
}

#[test]
fn a_run_past_the_end_of_its_text_is_stopped() {
    let (mut h, fw) = ready();
    fw.set_endless(true);
    h.send_cw(LONG).unwrap();
    assert!(fw.wait_receive(Duration::from_secs(2)));
    let (lasted, why) = off(&fw, 0);
    assert_eq!(why, Off::Stop);
    // At its text (0.29 s) and run_slack (0.1 s), give or take a keep-alive.
    assert!(
        lasted > Duration::from_millis(350) && lasted < Duration::from_secs(1),
        "{lasted:?}"
    );
    // Reported once, so that the piece is not counted as sent.
    let e = h.is_transmitting().unwrap_err().to_string();
    assert!(e.contains("past the end of its text"), "{e}");
    assert!(!h.is_transmitting().unwrap());
}

#[test]
fn a_run_cut_short_is_reported() {
    // 1.46 s of text, and a firmware limit of 1 s.
    let mut set = settings();
    set.time_scale = 10.0;
    let (mut h, fw) = ready_with(set);
    fw.set_hello(1, Duration::from_secs(1), MIN_LINK_TIMEOUT);
    h.send_cw(LONG).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    let e = loop {
        match h.is_transmitting() {
            Ok(true) if Instant::now() < end => thread::sleep(Duration::from_millis(20)),
            Ok(tx) => panic!("no error; transmitting {tx}"),
            Err(e) => break e.to_string(),
        }
    };
    assert!(e.contains("cut short"), "{e}");
    assert_eq!(off(&fw, 0).1, Off::Limit);
    assert!(!h.is_transmitting().unwrap());
}

#[test]
fn a_cut_link_is_ended_by_the_firmwares_link_timeout() {
    let (mut h, fw) = ready();
    fw.set_endless(true);
    h.send_cw(LONG).unwrap();
    fw.set_deaf(true);
    assert!(fw.wait_receive(Duration::from_secs(2)));
    assert_eq!(off(&fw, 0).1, Off::Link);
    assert!(h.is_transmitting().is_err(), "no reply: unknown");
}

#[test]
fn a_lost_cw_reply_is_followed_by_stop() {
    let (mut h, fw) = ready();
    fw.set_lose_cw_reply(true);
    assert!(matches!(h.send_cw(LONG), Err(RigError::Timeout)));
    assert_eq!(off(&fw, 0).1, Off::Stop);
    assert!(!lock(&h.shared.state).keyed);
}

#[test]
fn a_garbled_reply_is_asked_again() {
    let (mut h, fw) = ready();
    let before = fw.received().len();
    fw.garble_replies(1);
    assert!(!h.is_transmitting().unwrap());
    let sent: Vec<String> = fw.received()[before..]
        .iter()
        .map(|r| r.1.clone())
        .collect();
    assert_eq!(sent, ["STATUS", "STATUS"]);
}

#[test]
fn a_cw_just_after_a_stop_waits_for_the_firmwares_check() {
    let (mut h, fw) = ready();
    h.send_cw("E").unwrap();
    h.stop_cw().unwrap();
    let t0 = Instant::now();
    h.send_cw("TEST").unwrap();
    let waited = t0.elapsed();
    assert!(
        waited > super::mock::STOP_WATCH - Duration::from_millis(200),
        "{waited:?}"
    );
    // Its times from when it was taken: it runs to its end, not stopped early.
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    assert_eq!(off(&fw, 1).1, Off::Done);
    assert!(h.is_transmitting().is_ok());
    // Leading and trailing spaces are not sent.
    thread::sleep(super::mock::STOP_WATCH);
    h.send_cw(" HI ").unwrap();
    assert_eq!(fw.runs()[2].text, "HI");
}

#[test]
fn a_piece_longer_than_the_firmwares_limit_is_not_sent() {
    // 30 zeros at 6 wpm: 132 s.
    let set = Settings {
        time_scale: 1.0,
        ..settings()
    };
    let fw = firmware(1.0);
    let link = Link::new(Box::new(fw.clone()), set.reply_timeout);
    let mut h = Handheld::new(link, set, 6).unwrap();
    let e = h.send_cw(&"0".repeat(30)).unwrap_err().to_string();
    assert!(
        e.contains("longer than the firmware's transmit limit"),
        "{e}"
    );
    assert!(fw.runs().is_empty());
}

#[test]
fn the_switch_over_to_transmit_counts_against_the_firmwares_limit() {
    // 30 M at 6 wpm: 59.4 s of Morse, and up to 30 switch-overs on top.
    let set = Settings {
        time_scale: 1.0,
        ..settings()
    };
    let fw = firmware(1.0);
    let link = Link::new(Box::new(fw.clone()), set.reply_timeout);
    let mut h = Handheld::new(link, set, 6).unwrap();
    let text = "M".repeat(30);
    assert!(Duration::from_secs_f32(1.2 / 6.0) * cw::units(&text) <= Duration::from_secs(60));
    let e = h.send_cw(&text).unwrap_err().to_string();
    assert!(
        e.contains("lasts 61 s at 6 wpm, longer than the firmware's transmit limit"),
        "{e}"
    );
    assert!(fw.runs().is_empty());
}

#[test]
fn dropping_the_rig_stops_the_firmware() {
    let (mut h, fw) = ready();
    h.send_cw(LONG).unwrap();
    drop(h);
    assert_eq!(off(&fw, 0).1, Off::Stop);
}

#[test]
fn what_a_handheld_cannot_do_is_refused() {
    let (mut h, fw) = ready();
    assert!(h.set_transmit(true).is_err());
    assert!(h.start_tune().is_err());
    assert!(h.read_swr().is_err() && h.read_po().is_err());
    assert!(h.send_cw(&"E".repeat(MAX_CW_CHARS + 1)).is_err());
    assert!(h.send_cw("HI #1").is_err());
    assert!(h.send_cw(" ").is_err());
    assert!(h.set_key_speed(60).is_err());
    assert!(!h.has_tuner() && !h.has_meters());
    assert!(fw.runs().is_empty());
    h.send_cw("e").unwrap();
    assert_eq!(fw.runs()[0].text, "E");
    assert!(h.send_cw("E").is_err(), "one run at a time");
}

#[test]
fn settings_are_read_back_and_never_set() {
    let (mut h, fw) = ready();
    h.set_frequency(FREQ).unwrap();
    assert_eq!(h.frequency().unwrap(), FREQ);
    assert!(!h.split_or_delta_tx().unwrap());
    let e = h.set_frequency(144_070_000).unwrap_err().to_string();
    assert!(e.contains("set it to 144070000 Hz simplex"), "{e}");
    assert_eq!(fw.frequencies(), (FREQ, FREQ), "not changed");
    // `low` is any of the radio's low levels.
    h.set_rf_power_watts(40).unwrap();
    fw.set_power("LOW5");
    h.set_rf_power_watts(40).unwrap();
    fw.set_power("HIGH");
    let e = h.set_rf_power_watts(40).unwrap_err().to_string();
    assert!(e.contains("reads HIGH"), "{e}");
    fw.set_modes("CW", "FM");
    let e = h.set_mode_cw().unwrap_err().to_string();
    assert!(e.contains("CW transmit, FM receive"), "{e}");
    fw.set_modes("CW", "CW");
    h.set_mode_cw().unwrap();
    h.set_break_in(true).unwrap();
    fw.set_break_in(false);
    assert!(h.set_break_in(true).is_err());
    fw.set_tx_hz(144_670_000);
    assert!(h.split_or_delta_tx().unwrap());
    assert_eq!(h.transmit_frequency().unwrap(), 144_670_000);
    // Only queries went out.
    let sent: Vec<String> = fw.received().into_iter().map(|r| r.1).collect();
    assert!(
        sent.iter().all(
            |b| ["HELLO", "STOP", "STATUS", "FREQ", "MODE", "POWER", "BREAKIN"]
                .contains(&b.as_str())
        ),
        "{sent:?}"
    );
}

#[test]
fn a_refused_cw_is_explained() {
    let (mut h, fw) = ready();
    fw.set_refuse_tx(true);
    let e = h.send_cw("TEST").unwrap_err().to_string();
    assert!(e.contains("REFUSED (the radio would not transmit"), "{e}");
    assert!(fw.runs().is_empty());
    assert!(!lock(&h.shared.state).keyed);
    fw.set_refuse_tx(false);
    h.send_cw("TEST").unwrap();
}

#[test]
fn the_hang_test_is_ended_by_the_watchdog() {
    // Real-time Morse, so that the text outlasts the watchdog.
    let mut set = settings();
    set.time_scale = 1.0;
    let (mut h, fw) = ready_with(set.clone());
    fw.set_watchdog(Some(Duration::from_millis(100)));
    assert!(h.hang_test("E").is_err(), "too short to tell");
    assert!(fw.runs().is_empty());
    let HangTest::Hung { at, confirmed } = h.hang_test(LONG).unwrap() else {
        panic!("not hung")
    };
    assert!(confirmed);
    assert!(fw.hung() && fw.transmitting());
    drop(h);
    assert!(fw.wait_receive(Duration::from_secs(1)));
    assert_eq!(off(&fw, 0).1, Off::Watchdog);
    // Nothing sent once hung, not even the STOP of dropping it: the firmware
    // would have taken that once it was back.
    let sent: Vec<String> = fw.received().into_iter().map(|r| r.1).collect();
    assert_eq!(sent.last().map(String::as_str), Some("TEST HANG"));
    // Opened again: found restarted, about the watchdog's time after the hang, and
    // on receive.
    let mut h = open(&fw, set.clone()).unwrap();
    let restarted = h.started_at().unwrap().saturating_duration_since(at);
    assert!(
        restarted > Duration::from_millis(50) && restarted < Duration::from_millis(300),
        "{restarted:?}"
    );
    assert!(!h.is_transmitting().unwrap());

    // A firmware without the hang test: refused, stopped, and reported as keyed,
    // since the CW went out.
    let text = h.hang_test_text().unwrap();
    let (mut h, fw) = ready_with(set.clone());
    fw.set_no_hang_test(true);
    match h.hang_test(&text).unwrap() {
        HangTest::Failed { keyed, error } => {
            assert!(keyed);
            assert!(format!("{error:#}").contains("UNKNOWN"), "{error:#}");
        }
        HangTest::Hung { .. } => panic!("hung"),
    }
    assert!(!fw.hung() && !fw.transmitting());
    assert_eq!(off(&fw, 0).1, Off::Stop);
    // A CW refused outright: nothing keyed.
    let (mut h, fw) = ready_with(set.clone());
    fw.set_refuse_tx(true);
    match h.hang_test(&text).unwrap() {
        HangTest::Failed { keyed, error } => {
            assert!(!keyed, "{error:#}");
            assert!(format!("{error:#}").contains("REFUSED"), "{error:#}");
        }
        HangTest::Hung { .. } => panic!("hung"),
    }
    assert!(!fw.hung() && !fw.transmitting());
    // Hang text at any speed the node allows.
    for wpm in [5, 20, 50] {
        let set = Settings {
            time_scale: 1.0,
            ..settings()
        };
        let link = Link::new(Box::new(firmware(1.0)), set.reply_timeout);
        let h = Handheld::new(link, set, wpm).unwrap();
        let text = h.hang_test_text().unwrap();
        assert!(text.len() <= MAX_CW_CHARS, "{wpm} {text}");
        assert!(h.set.dot(wpm) * cw::units(&text) >= WATCHDOG_RESET + Duration::from_secs(2));
    }
}

#[test]
fn the_duty_cycle_waits_for_earlier_runs_to_leave_the_window() {
    let mut set = settings();
    set.duty = 0.5;
    set.duty_window = Duration::from_secs(10);
    let (h, _fw) = ready_with(set);
    let now = Instant::now();
    // 4 s on the air, ending 1 s ago: 1 s of the 5 s budget left.
    lock(&h.shared.state)
        .on_air
        .push_back((now - Duration::from_secs(5), now - Duration::from_secs(1)));
    assert_eq!(
        h.duty_rest(Duration::from_millis(500)).unwrap(),
        Duration::ZERO
    );
    // 3 s needs 2 s of the earlier run out of the window: the window's start
    // must pass 3 s ago, 7 s from now.
    let rest = h.duty_rest(Duration::from_secs(3)).unwrap();
    assert!(
        rest.abs_diff(Duration::from_secs(7)) < Duration::from_millis(100),
        "{rest:?}"
    );
    // More than the whole budget can never go.
    assert!(h.duty_rest(Duration::from_secs(6)).is_err());
}

#[test]
fn a_busy_frequency_is_waited_for_and_then_given_up_on() {
    let mut set = settings();
    set.busy_quiet = Duration::from_millis(100);
    let (mut h, fw) = ready_with(set);
    // Quiet only since the firmware started.
    assert!(h.rest_needed(Duration::ZERO).unwrap() > Duration::ZERO);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(h.rest_needed(Duration::ZERO).unwrap(), Duration::ZERO);
    fw.set_busy_for(Duration::from_millis(200));
    // In use now: the whole quiet time to wait.
    assert_eq!(
        h.rest_needed(Duration::ZERO).unwrap(),
        Duration::from_millis(100)
    );
    // Still busy past busy_max_wait: give up.
    let end = Instant::now() + Duration::from_secs(2);
    let err = loop {
        fw.set_busy_for(Duration::from_millis(200));
        match h.rest_needed(Duration::ZERO) {
            Ok(_) if Instant::now() < end => thread::sleep(Duration::from_millis(20)),
            Ok(_) => panic!("never gave up"),
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("in use"), "{err}");
    // Quiet again: fine.
    thread::sleep(Duration::from_millis(350));
    assert_eq!(h.rest_needed(Duration::ZERO).unwrap(), Duration::ZERO);
}

#[test]
fn the_link_test_passes_only_if_the_firmware_stops_by_itself() {
    let mut set = settings();
    // Slow enough (2.9 s) for the text to outlast the link timeout by a second.
    set.time_scale = 5.0;
    let (mut h, fw) = ready_with(set.clone());
    h.link_test(LONG).unwrap();
    assert_eq!(off(&fw, 0).1, Off::Link);

    let (mut h, fw) = ready_with(set.clone());
    fw.set_no_link_watchdog(true);
    let e = h.link_test(LONG).unwrap_err();
    assert!(e.to_string().contains("did not stop it"), "{e}");
    assert_eq!(off(&fw, 0).1, Off::Stop);
    assert!(h.link_test("E").is_err(), "too short to tell");
    // A transmit limit as short as the link timeout would pass it for the wrong
    // reason.
    let fw = firmware(5.0);
    fw.set_hello(1, Duration::from_secs(2), MIN_LINK_TIMEOUT);
    let mut h = open(&fw, set).unwrap();
    let e = h.link_test(LONG).unwrap_err();
    assert!(e.to_string().contains("could not tell"), "{e}");
    assert!(fw.runs().is_empty(), "nothing keyed");
}

/// The station's timing, sped up like the firmware's keyer.
fn station_cfg() -> StationConfig {
    StationConfig {
        frequency_hz: FREQ,
        power_watts: 5,
        key_speed_wpm: 20,
        max_key: Duration::from_secs(45).div_f32(SCALE),
        swr_limit: 2.0,
        segment_pause: Duration::from_millis(10),
        swr_delay: Duration::from_millis(1),
        swr_window: Duration::from_millis(20),
        swr_min_po: 1.0,
        break_in_delay_dots: 10.0,
        stuck_margin: Duration::from_secs(3).div_f32(SCALE),
        tune_timeout: Duration::from_secs(1),
        poll: Duration::from_millis(2),
        station_id: "DE N0DE".into(),
        id_interval: ID_INTERVAL.div_f32(SCALE),
        po_limit: po_limit(5),
        duty: duty_for_power(5),
        duty_window: DUTY_WINDOW.div_f32(SCALE),
        max_transmission: MAX_TRANSMISSION.div_f32(SCALE),
        radio_wait: Duration::from_secs(10),
    }
}

fn station_with(set: Settings) -> (Station<Handheld>, MockFirmware) {
    station_cfg_with(set, station_cfg())
}

fn station_cfg_with(set: Settings, cfg: StationConfig) -> (Station<Handheld>, MockFirmware) {
    let fw = firmware(SCALE);
    let st = Station::new(open(&fw, set).unwrap(), cfg, None);
    st.configure().unwrap();
    (st, fw)
}

fn tx(segments: &[&str]) -> Transmission {
    Transmission {
        segments: segments.iter().map(|s| s.to_string()).collect(),
        read_ids: Vec::new(),
    }
}

#[test]
fn the_station_keys_a_handheld_without_tuning_or_reading_meters() {
    let (mut st, fw) = station_with(settings());
    // A window start tunes nothing and keys nothing, not even an ID.
    st.start_window().unwrap();
    st.open_window().unwrap();
    assert!(fw.runs().is_empty());
    st.transmit(&tx(&["R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K"]))
        .unwrap();
    // One run per keyer piece, each ended by the text running out.
    let runs = fw.runs();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.off.unwrap().1 == Off::Done));
    assert!(!fw.transmitting());
    assert!(!st.tx_inhibited());
}

#[test]
fn the_station_stops_firmware_that_keeps_sending() {
    let mut set = settings();
    // The rig's own deadline out of the way, to test the station's.
    set.run_slack = Duration::from_secs(3600);
    set.max_run = Duration::from_secs(3600);
    let (mut st, fw) = station_with(set);
    fw.set_endless(true);
    assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Stuck));
    assert_eq!(off(&fw, 0).1, Off::Stop);
    assert!(!st.tx_inhibited(), "stopped, so not inhibited");
}

#[test]
fn firmware_that_overruns_fails_the_transmission_even_once_stopped() {
    let mut set = settings();
    // The rig's own stop (within a keep-alive, 250 ms, of the end of the text)
    // comes before the station's stuck check, as on the air.
    set.run_slack = Duration::from_millis(20);
    let mut cfg = station_cfg();
    cfg.stuck_margin = Duration::from_secs(1);
    let (mut st, fw) = station_cfg_with(set, cfg);
    fw.set_endless(true);
    let e = st.transmit(&tx(&["TEST", "MORE"])).unwrap_err();
    assert!(e.to_string().contains("past the end"), "{e}");
    assert_eq!(fw.runs().len(), 1, "the next piece not keyed");
    assert_eq!(off(&fw, 0).1, Off::Stop);
    assert!(!st.tx_inhibited());
}

#[test]
fn firmware_that_will_not_stop_inhibits_transmitting() {
    let mut set = settings();
    set.run_slack = Duration::from_secs(3600);
    set.max_run = Duration::from_secs(3600);
    let (mut st, fw) = station_with(set);
    fw.set_hello(1, Duration::from_secs(1), MIN_LINK_TIMEOUT);
    fw.set_endless(true);
    fw.set_ignore_stop(true);
    fw.set_no_link_watchdog(true);
    assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
    assert!(st.tx_inhibited());
    // The station goes on sending STOP; the firmware's check of the first one has
    // its watchdog reset the radio all the same.
    let received = fw.received();
    let keyed = received
        .iter()
        .position(|r| r.1.starts_with("CW "))
        .unwrap();
    let first_stop = received[keyed..]
        .iter()
        .find(|r| r.1 == "STOP")
        .expect("a STOP after keying")
        .0;
    assert!(fw.wait_receive(super::mock::STOP_RESET + Duration::from_secs(1)));
    let r = &fw.runs()[0];
    let (at, why) = r.off.unwrap();
    assert_eq!(why, Off::Watchdog);
    assert!(
        at <= first_stop + super::mock::STOP_RESET,
        "{:?}",
        at - first_stop
    );
}

#[test]
fn a_radio_set_otherwise_is_not_keyed() {
    let (mut st, fw) = station_with(settings());
    // A repeater offset, or the wrong frequency: refused, and left as it is.
    fw.set_tx_hz(FREQ + 600_000);
    assert!(st.check().is_err());
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    assert_eq!(fw.frequencies(), (FREQ, FREQ + 600_000));
    fw.set_frequencies(FREQ + 10_000, FREQ + 10_000);
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    fw.set_frequencies(FREQ, FREQ);
    // Not in CW, break-in off, the wrong power: the same.
    fw.set_modes("FM", "FM");
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    fw.set_modes("CW", "CW");
    fw.set_break_in(false);
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    fw.set_break_in(true);
    fw.set_power("HIGH");
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    assert!(fw.runs().is_empty());
    // The node's own checks refused them: not one CW was sent.
    assert!(fw.received().iter().all(|r| !r.1.starts_with("CW ")));
    assert!(!st.tx_inhibited());
    fw.set_power("LOW2");
    st.transmit(&tx(&["TEST"])).unwrap();
    assert_eq!(fw.runs().len(), 1);
}

#[test]
fn a_handheld_keyed_by_hand_is_not_keyed_over() {
    let (mut st, fw) = station_with(settings());
    fw.key_by_hand();
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    assert!(fw.runs().iter().all(|r| r.text == "PTT"));
    // Forced back to receive, and so not inhibited.
    assert_eq!(off(&fw, 0).1, Off::Stop);
    assert!(!st.tx_inhibited());
}

#[test]
fn a_storm_hold_keeps_a_handheld_off_the_air() {
    let fw = firmware(SCALE);
    let mut st = Station::new(open(&fw, settings()).unwrap(), station_cfg(), None);
    let hold = StormHold::new(Duration::from_secs(60));
    st.set_storm_hold(hold.clone());
    st.configure().unwrap();
    hold.set(Some("thunder forecast".into()));
    assert_eq!(
        st.transmit(&tx(&["TEST"])),
        Err(TxError::Storm("thunder forecast".into()))
    );
    assert!(fw.runs().is_empty());
    hold.set(None);
    st.transmit(&tx(&["TEST"])).unwrap();
    assert_eq!(fw.runs().len(), 1);
}

#[test]
fn the_duty_cycle_paces_a_long_transmission_on_receive() {
    let mut set = settings();
    // 0.5 s on the air (real) in any 1 s.
    set.duty = 0.5;
    set.duty_window = Duration::from_secs(1);
    let (mut st, fw) = station_with(set);
    let t0 = Instant::now();
    st.transmit(&tx(&[
        "PART ONE OF A REPLY THAT GOES ON",
        "PART TWO OF A REPLY THAT GOES ON",
        "PART THREE OF A REPLY THAT ENDS",
    ]))
    .unwrap();
    let took = t0.elapsed();
    let runs: Vec<(Instant, Instant)> =
        fw.runs().iter().map(|r| (r.on, r.off.unwrap().0)).collect();
    let on_air: Duration = runs.iter().map(|&(s, e)| e - s).sum();
    // Six pieces of about 0.17 s each do not fit in one window's 0.5 s, so some
    // waited.
    assert_eq!(runs.len(), 6);
    assert!(on_air > Duration::from_millis(700), "{on_air:?}");
    assert!(
        took > on_air + Duration::from_millis(300),
        "{took:?} {on_air:?}"
    );
    // Never more than the budget in any window.
    let budget = Duration::from_millis(500 + 30);
    for &(_, end) in &runs {
        let from = end - Duration::from_secs(1);
        let used: Duration = runs
            .iter()
            .map(|&(s, e)| e.min(end).saturating_duration_since(s.max(from)))
            .sum();
        assert!(
            used <= budget,
            "{used:?} in the window ending at a run's end"
        );
    }
}

#[test]
fn ids_stay_on_time_while_the_duty_cycle_holds_a_long_transmission() {
    ids_stay_on_time_under_the_duty_cycle(Duration::ZERO);
}

/// Runs measured longer than their Morse, as on a radio (the switch-over to
/// transmit, the break-in tail) or a slow machine, take more of the duty cycle than
/// the station's count of them: the room left for the next ID must allow for that.
#[test]
fn ids_stay_on_time_under_the_duty_cycle_with_runs_that_overrun() {
    // 15 ms made an ID late here before the station allowed for it.
    for ms in [15, 30] {
        ids_stay_on_time_under_the_duty_cycle(Duration::from_millis(ms));
    }
}

/// The most the whole test process may stop, in any [`PAUSE_WINDOW`] of a
/// transmission, for the ID test to judge it: under the least slack the test's
/// timing leaves, the 100 ms `run_slack` the node allows past a run's text less the
/// 30 ms overrun (a longer stop as a run ends has the node stop it for going on too
/// long), with room for the node's own polling and the firmware's replies. The IDs
/// leave more: on an idle machine each ends about 1.4 s after the one before, for
/// the 1.6 s allowed.
const PAUSE_LIMIT: Duration = Duration::from_millis(50);
/// Longer than anything the ID test times: an ID interval with the 100 ms it
/// allows (1.6 s), or a run up to the node's deadline for it.
const PAUSE_WINDOW: Duration = Duration::from_secs(2);
/// How often the pause meter looks at the clock.
const PAUSE_TICK: Duration = Duration::from_millis(5);
/// The shortest pause the meter keeps: shorter ones are a busy machine waking its
/// thread a little late, which would otherwise add up to [`PAUSE_LIMIT`] in a
/// window on a CI runner running the other tests alongside, and leave no run
/// judged before the last.
const PAUSE_FLOOR: Duration = Duration::from_millis(20);
/// Times the ID test runs a transmission again after one the machine paused in.
/// The last run is judged, paused or not.
const NOT_JUDGED_RERUNS: usize = 2;

/// Measures how long the whole test process stops running, as the self-test's
/// pause meter does (`selftest::any_radio::PauseMeter`): a thread that looks at the
/// clock every [`PAUSE_TICK`] and keeps the gaps past that. Nothing the node does
/// holds it up; only the machine can (a CI runner stops the whole process for
/// tens to hundreds of milliseconds now and then). It cannot see a stop that holds
/// up only the node's threads: a failure with no pause measured may be one.
struct PauseMeter {
    stop: Arc<AtomicBool>,
    /// Every pause of [`PAUSE_FLOOR`] or more: when it ended, and how long.
    thread: Option<JoinHandle<Vec<(Instant, Duration)>>>,
}

impl PauseMeter {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut pauses = Vec::new();
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(PAUSE_TICK);
                    let now = Instant::now();
                    let gap = (now - last).saturating_sub(PAUSE_TICK);
                    if gap >= PAUSE_FLOOR {
                        pauses.push((now, gap));
                    }
                    last = now;
                }
                pauses
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stop measuring: every pause of [`PAUSE_FLOOR`] or more, as (when it ended,
    /// how long).
    fn pauses(mut self) -> Vec<(Instant, Duration)> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for PauseMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// The most of `pauses` (each as when it ended, and how long) in any `window`: the
/// most lies in a window that ends where a pause ends, or starts where one starts.
fn most_paused_in(pauses: &[(Instant, Duration)], window: Duration) -> Duration {
    let spans: Vec<(Instant, Instant)> = pauses
        .iter()
        .map(|&(end, gap)| (end.checked_sub(gap).unwrap_or(end), end))
        .collect();
    let within = |from: Instant, to: Instant| -> Duration {
        spans
            .iter()
            .map(|&(s, e)| e.min(to).saturating_duration_since(s.max(from)))
            .sum()
    };
    spans
        .iter()
        .flat_map(|&(s, e)| {
            [
                within(e.checked_sub(window).unwrap_or(e), e),
                within(s, s + window),
            ]
        })
        .max()
        .unwrap_or_default()
}

/// Runs the ID test's transmission until one is judged: a run the machine paused in
/// for [`PAUSE_LIMIT`] or more in any [`PAUSE_WINDOW`] is not judged and runs
/// again, up to [`NOT_JUDGED_RERUNS`] times; the last run is judged whatever it
/// measured. A paused run that passed is not judged either: here a pause can favour
/// the node as well as hold it up (one just before the station weighs an ID has it
/// key the ID a piece sooner, and one between the station noting an ID's start and
/// the radio keying it shortens the next interval as measured). Each run's pauses
/// are printed; those of a run not judged also when the test passes.
fn ids_stay_on_time_under_the_duty_cycle(overrun: Duration) {
    for run in 0..=NOT_JUDGED_RERUNS {
        let (result, pauses) = ids_on_time_once(overrun);
        let longest = pauses.iter().map(|p| p.1).max().unwrap_or_default();
        let most = most_paused_in(&pauses, PAUSE_WINDOW);
        let paused = most >= PAUSE_LIMIT;
        let measured = format!(
            "overrun {overrun:?}, run {} of at most {}: the test process stopped {} times, \
             longest {:.3} s, {:.3} s in any {:.0} s (limit {:.3} s)",
            run + 1,
            NOT_JUDGED_RERUNS + 1,
            pauses.len(),
            longest.as_secs_f32(),
            most.as_secs_f32(),
            PAUSE_WINDOW.as_secs_f32(),
            PAUSE_LIMIT.as_secs_f32()
        );
        let judged = !paused || run == NOT_JUDGED_RERUNS;
        let line = format!(
            "{measured}: {}, {}",
            match (judged, paused) {
                (false, _) => "not judged",
                (true, false) => "judged",
                (true, true) => "judged, as the last run allowed",
            },
            match &result {
                Ok(ends) => format!("passed: {ends}"),
                Err(e) => format!("failed: {e}"),
            }
        );
        println!("{line}");
        if !judged {
            // Past the test harness's capture, so that a CI log shows runs set aside
            // even when the test passes.
            let _ = writeln!(std::io::stderr(), "{line}");
            continue;
        }
        if let Err(e) = result {
            panic!("{e} ({measured})");
        }
        return;
    }
}

/// One transmission of the ID test, with the pauses of the test process while it
/// was under way: an error says what was late, or what failed.
fn ids_on_time_once(overrun: Duration) -> (Result<String, String>, Vec<(Instant, Duration)>) {
    let mut set = settings();
    // 0.5 s on the air (real) in any 1 s, and an ID due every 1.5 s.
    set.duty = 0.5;
    set.duty_window = Duration::from_secs(1);
    let mut cfg = station_cfg();
    cfg.id_interval = Duration::from_millis(1500);
    let interval = cfg.id_interval;
    let (mut st, fw) = station_cfg_with(set, cfg);
    fw.set_overrun(overrun);
    let mut segments: Vec<String> = (0..12)
        .map(|i| format!("TEST TEST TEST = {}", (b'A' + i) as char))
        .collect();
    segments.last_mut().unwrap().push_str(" DE N0DE K");
    let meter = PauseMeter::start();
    let start = Instant::now();
    let sent = st.transmit(&Transmission {
        segments,
        read_ids: Vec::new(),
    });
    let done = Instant::now();
    // Only while the transmission was under way.
    let pauses = meter
        .pauses()
        .into_iter()
        .filter_map(|(end, gap)| {
            let from = end.checked_sub(gap).unwrap_or(end).max(start);
            let to = end.min(done);
            (to > from).then(|| (to, to - from))
        })
        .collect();
    (on_time(sent, &fw.runs(), start, interval), pauses)
}

/// Whether a transmission `sent` from `start` went out whole, and keyed the ID
/// (or its end) within `interval` of its start and of each ID before: how long
/// after each the next ended, or what was late.
fn on_time(
    sent: Result<(), TxError>,
    runs: &[super::mock::Run],
    start: Instant,
    interval: Duration,
) -> Result<String, String> {
    sent.map_err(|e| format!("the transmission failed: {e:?}"))?;
    let ids = runs.iter().filter(|r| r.text == "DE N0DE").count();
    if ids < 2 {
        return Err(format!("{ids} IDs in {:?}", start.elapsed()));
    }
    // From the start, and from each ID, the next ID (or the end) is keyed within
    // the interval, rests on receive included.
    let mut since = start;
    let mut ends = Vec::new();
    for (i, r) in runs.iter().enumerate() {
        if r.text == "DE N0DE" || i + 1 == runs.len() {
            let end = r.off.ok_or(format!("run {i} {:?} never ended", r.text))?.0;
            if end > since + interval + Duration::from_millis(100) {
                return Err(format!(
                    "run {i} {:?} ends {:?} after the last ID",
                    r.text,
                    end - since
                ));
            }
            ends.push(format!("{:.3} s", (end - since).as_secs_f32()));
            since = r.on;
        }
    }
    Ok(format!(
        "{ids} IDs; each, and the end, ended {} after the start or the ID before",
        ends.join(", ")
    ))
}

#[test]
fn stages_gate_keying_and_running() {
    assert!(check_stage(Stage::None, Action::Key).is_err());
    assert!(check_stage(Stage::Listen, Action::Key).is_ok());
    assert!(check_stage(Stage::Listen, Action::Hang).is_err());
    assert!(check_stage(Stage::Keying, Action::Hang).is_ok());
    assert!(check_stage(Stage::Keying, Action::Run).is_err());
    assert!(check_stage(Stage::Done, Action::Run).is_ok());
}

#[test]
fn bands_are_the_handhelds_amateur_bands() {
    assert!(band_of(144_000_000).is_some());
    assert!(band_of(146_520_000).is_some());
    assert!(band_of(148_000_001).is_none());
    assert!(band_of(162_550_000).is_none(), "weather radio");
    assert!(band_of(446_000_000).is_some());
}
