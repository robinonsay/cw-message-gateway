use super::mock::{MockFirmware, Off};
use super::*;
use crate::session::Transmission;
use crate::station::{Station, StationConfig, TxError};
use crate::storm::StormHold;

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
    for (version, tx_limit, link) in [
        (2, 60_000, 100),
        (1, 0, 100),
        (1, 61_000, 100),
        (1, 60_000, 500),
        (1, 60_000, 5000),
    ] {
        let fw = firmware(SCALE);
        fw.set_hello(
            version,
            Duration::from_millis(tx_limit),
            Duration::from_millis(link),
        );
        assert!(
            open(&fw, settings()).is_err(),
            "{version} {tx_limit} {link}"
        );
    }
    let fw = firmware(SCALE);
    fw.set_silent(true);
    let e = open(&fw, settings()).err().unwrap();
    assert!(
        format!("{e:#}").contains("no CW firmware answering"),
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
    fw.garble_replies(1);
    assert!(!h.is_transmitting().unwrap());
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
fn frequency_power_and_mode_are_set_and_read_back() {
    let (mut h, fw) = ready();
    h.set_frequency(144_070_000).unwrap();
    assert_eq!(fw.frequencies(), (144_070_000, 144_070_000));
    assert_eq!(h.frequency().unwrap(), 144_070_000);
    assert!(!h.split_or_delta_tx().unwrap());
    assert!(h.set_frequency(146_000).is_err(), "refused by the firmware");
    h.set_rf_power_watts(40).unwrap();
    assert_eq!(fw.power(), "LOW", "the configured level, not the watts");
    fw.set_tx_hz(144_670_000);
    assert!(h.split_or_delta_tx().unwrap());
    assert_eq!(h.transmit_frequency().unwrap(), 144_670_000);
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
    assert_eq!(fw.frequencies(), (FREQ, FREQ));
    assert!(fw.mode_cw());
    assert_eq!(fw.power(), "LOW");
    // A window start tunes nothing and keys nothing.
    st.start_window().unwrap();
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
fn the_station_watchdog_stops_firmware_that_keeps_sending() {
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
    // Its own transmit limit ends it.
    assert!(fw.wait_receive(Duration::from_secs(2)));
    assert_eq!(off(&fw, 0).1, Off::Limit);
}

#[test]
fn a_transmit_frequency_off_the_set_one_is_refused() {
    let (mut st, fw) = station_with(settings());
    // Set apart by hand: the node puts it back.
    fw.set_tx_hz(FREQ + 600_000);
    st.check().unwrap();
    assert_eq!(fw.frequencies(), (FREQ, FREQ));
    // Kept apart by the radio, as a repeater offset: refused.
    fw.set_tx_offset(600_000);
    assert!(st.check().is_err());
    assert!(st.transmit(&tx(&["TEST"])).is_err());
    assert!(fw.runs().is_empty());
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
fn stages_gate_keying_and_running() {
    assert!(check_stage(Stage::None, Action::Key).is_err());
    assert!(check_stage(Stage::Listen, Action::Key).is_ok());
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
