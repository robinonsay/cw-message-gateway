use super::playback::MockPlayback;
use super::ptt::MockPtt;
use super::*;
use crate::session::Transmission;
use crate::station::{Station, StationConfig, TxError};

/// Audio plays this many times faster than real time.
const SCALE: f32 = 50.0;
const RATE: u32 = 8000;

fn settings() -> Settings {
    Settings {
        tone_hz: 800.0,
        tone_level: 0.4,
        lead_in: Duration::from_millis(400),
        tail: Duration::from_millis(150),
        max_run: Duration::from_secs(2),
        playback_slack: Duration::from_millis(200),
        duty: 1.0,
        duty_window: Duration::from_secs(60),
        busy_quiet: Duration::from_millis(100),
        busy_max_wait: Duration::from_millis(500),
        poll: Duration::from_millis(2),
        time_scale: SCALE,
    }
}

fn rig_with(set: Settings) -> (Handheld, MockPtt, MockPlayback) {
    let (ptt, out) = (MockPtt::new(), MockPlayback::new(RATE, SCALE));
    let h = Handheld::new(
        Box::new(ptt.clone()),
        Arc::new(out.clone()),
        set,
        146_580_000,
        20,
    )
    .unwrap();
    (h, ptt, out)
}

fn rig() -> (Handheld, MockPtt, MockPlayback) {
    rig_with(settings())
}

fn wait_receive(h: &mut Handheld, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !h.is_transmitting().unwrap() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Index of the first sample louder than silence.
fn first_sound(s: &[f32]) -> usize {
    s.iter().position(|x| x.abs() > 1e-3).unwrap_or(s.len())
}

#[test]
fn a_run_keys_plays_the_tone_after_the_lead_in_and_releases() {
    let (mut h, ptt, out) = rig();
    assert!(!ptt.keyed(), "released at start");
    h.send_cw("CQ DE N0DE").unwrap();
    assert!(h.is_transmitting().unwrap());
    assert!(ptt.keyed());
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    assert!(!ptt.keyed());
    let played = out.played();
    assert_eq!(played.len(), 1);
    let p = &played[0];
    // Lead-in silence, then the tone (whose first samples are its rising edge);
    // the tail is silence too.
    let lead = (0.4 * RATE as f32) as usize;
    assert!((lead..lead + 4).contains(&first_sound(&p.samples)));
    let tail = p.samples.iter().rev().position(|x| x.abs() > 1e-3).unwrap();
    assert!(tail >= (0.15 * RATE as f32) as usize - 1, "{tail}");
    // The whole Morse fits: 20 wpm, PARIS timing.
    let tone = p.samples.len() - (0.55 * RATE as f32) as usize;
    let want = cw::duration_ms("CQ DE N0DE", 20) as usize * RATE as usize / 1000;
    assert!(
        tone.abs_diff(want) < RATE as usize / 100,
        "{tone} vs {want}"
    );
    // PTT went up before the audio started and came down after it ended.
    let ev = ptt.events();
    let keyed_at = ev.iter().find(|e| e.1).unwrap().0;
    let released_at = ev.iter().rev().find(|e| !e.1).unwrap().0;
    assert!(keyed_at <= p.started);
    assert!(released_at >= p.started + out.real(p.samples.len()));
    assert!(p.stopped.is_none(), "played to the end");
}

#[test]
fn stopping_releases_at_once() {
    let (mut h, ptt, out) = rig();
    h.send_cw("THIS IS A LONG ONE TO STOP").unwrap();
    h.stop_cw().unwrap();
    assert!(!h.is_transmitting().unwrap());
    assert!(!ptt.keyed());
    assert!(out.played()[0].stopped.is_some(), "audio stopped");
    // The run's worker does not key or release anything afterwards.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(ptt.events().iter().filter(|e| e.1).count(), 1);
    // And the next run goes out normally.
    h.send_cw("E").unwrap();
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
}

#[test]
fn a_hung_output_is_released_once_it_should_have_ended() {
    let (mut h, ptt, out) = rig();
    out.set_hang(true);
    h.send_cw("E").unwrap();
    let length = out.real(out.played()[0].samples.len());
    let t0 = Instant::now();
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    let took = t0.elapsed();
    assert!(took + Duration::from_millis(5) >= length + Duration::from_millis(200));
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert!(!ptt.keyed());
}

#[test]
fn the_deadman_releases_when_nothing_else_does() {
    let mut set = settings();
    // The run's own worker would wait an hour, and the deadman 300 ms.
    set.playback_slack = Duration::from_secs(3600);
    set.max_run = Duration::from_millis(300);
    let (mut h, ptt, out) = rig_with(set);
    out.set_hang(true);
    h.send_cw("E").unwrap();
    let t0 = Instant::now();
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    assert!(t0.elapsed() >= Duration::from_millis(250));
    assert!(!ptt.keyed());
}

#[test]
fn a_release_that_fails_leaves_it_keyed_until_one_works() {
    let mut set = settings();
    set.max_run = Duration::from_millis(100);
    let (mut h, ptt, _out) = rig_with(set);
    h.send_cw("TEST").unwrap();
    ptt.fail_releases(5);
    assert!(h.stop_cw().is_err());
    assert!(h.is_transmitting().unwrap(), "not known to be released");
    assert!(ptt.keyed());
    // The worker and the deadman keep trying until a release goes through.
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    assert!(!ptt.keyed());
}

#[test]
fn a_ptt_that_cannot_be_keyed_is_left_released() {
    let (mut h, ptt, out) = rig();
    ptt.fail_key(true);
    assert!(h.send_cw("TEST").is_err());
    assert!(!h.is_transmitting().unwrap());
    assert!(out.played().is_empty(), "no audio without PTT");
}

#[test]
fn an_output_that_cannot_start_releases_ptt() {
    let (mut h, ptt, out) = rig();
    out.set_fail_start(true);
    assert!(h.send_cw("TEST").is_err());
    assert!(!h.is_transmitting().unwrap());
    assert!(!ptt.keyed());
}

#[test]
fn what_a_handheld_cannot_do_is_refused() {
    let (mut h, ptt, _out) = rig();
    assert!(h.set_transmit(true).is_err());
    assert!(h.start_tune().is_err());
    assert!(h.read_swr().is_err() && h.read_po().is_err());
    assert!(h.send_cw(&"E".repeat(MAX_CW_CHARS + 1)).is_err());
    assert!(h.send_cw("HI #1").is_err());
    assert!(!h.has_tuner() && !h.has_meters());
    h.send_cw("E").unwrap();
    assert!(h.send_cw("E").is_err(), "one run at a time");
    assert!(wait_receive(&mut h, Duration::from_secs(2)));
    assert!(ptt.events().iter().all(|e| e.0 <= Instant::now()));
}

#[test]
fn dropping_the_rig_releases_ptt() {
    let (mut h, ptt, _out) = rig();
    h.send_cw("THIS IS A LONG ONE TO DROP").unwrap();
    drop(h);
    assert!(!ptt.keyed());
}

#[test]
fn the_duty_cycle_waits_for_earlier_runs_to_leave_the_window() {
    let mut set = settings();
    set.duty = 0.5;
    set.duty_window = Duration::from_secs(10);
    let (h, _ptt, _out) = rig_with(set);
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
        rest.abs_diff(Duration::from_secs(7)) < Duration::from_millis(30),
        "{rest:?}"
    );
    // More than the whole budget can never go.
    assert!(h.duty_rest(Duration::from_secs(6)).is_err());
}

#[test]
fn a_busy_frequency_is_waited_for_and_then_given_up_on() {
    let (mut h, _ptt, _out) = rig();
    let ch = ChannelMonitor::new(0.05);
    h.set_channel_monitor(ch.clone());
    let quiet = Block {
        at: Instant::now(),
        samples: vec![0.001; 400],
    };
    ch.observe(&quiet);
    assert_eq!(h.rest_needed(Duration::ZERO).unwrap(), Duration::ZERO);
    let loud = || Block {
        at: Instant::now(),
        samples: vec![0.3; 400],
    };
    ch.observe(&loud());
    let rest = h.rest_needed(Duration::ZERO).unwrap();
    assert!(rest > Duration::from_millis(50) && rest <= Duration::from_millis(100));
    // Still busy past busy_max_wait: give up.
    let end = Instant::now() + Duration::from_secs(2);
    let err = loop {
        ch.observe(&loud());
        match h.rest_needed(Duration::ZERO) {
            Ok(_) if Instant::now() < end => thread::sleep(Duration::from_millis(20)),
            Ok(_) => panic!("never gave up"),
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("in use"), "{err}");
    // Quiet again: fine.
    thread::sleep(Duration::from_millis(120));
    assert_eq!(h.rest_needed(Duration::ZERO).unwrap(), Duration::ZERO);
}

/// The station's timing, sped up like the rig's audio.
fn station_cfg() -> StationConfig {
    StationConfig {
        frequency_hz: 146_580_000,
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

/// [`Handheld`] with its dot length scaled like its audio, as the self-test does.
struct Scaled(Handheld);

impl Rig for Scaled {
    fn frequency(&mut self) -> civ::Result<u64> {
        self.0.frequency()
    }
    fn set_frequency(&mut self, hz: u64) -> civ::Result<()> {
        self.0.set_frequency(hz)
    }
    fn set_mode_cw(&mut self) -> civ::Result<()> {
        self.0.set_mode_cw()
    }
    fn set_rf_power_watts(&mut self, w: u32) -> civ::Result<()> {
        self.0.set_rf_power_watts(w)
    }
    fn set_key_speed(&mut self, wpm: u32) -> civ::Result<()> {
        self.0.set_key_speed(wpm)
    }
    fn set_break_in(&mut self, on: bool) -> civ::Result<()> {
        self.0.set_break_in(on)
    }
    fn set_break_in_delay(&mut self, dots: f32) -> civ::Result<()> {
        self.0.set_break_in_delay(dots)
    }
    fn dot_duration(&mut self) -> civ::Result<Duration> {
        Ok(self.0.dot_duration()?.div_f32(SCALE))
    }
    fn start_tune(&mut self) -> civ::Result<()> {
        self.0.start_tune()
    }
    fn tuner_busy(&mut self) -> civ::Result<bool> {
        self.0.tuner_busy()
    }
    fn read_swr(&mut self) -> civ::Result<f32> {
        self.0.read_swr()
    }
    fn read_po(&mut self) -> civ::Result<f32> {
        self.0.read_po()
    }
    fn send_cw(&mut self, text: &str) -> civ::Result<()> {
        self.0.send_cw(text)
    }
    fn stop_cw(&mut self) -> civ::Result<()> {
        self.0.stop_cw()
    }
    fn is_transmitting(&mut self) -> civ::Result<bool> {
        self.0.is_transmitting()
    }
    fn set_transmit(&mut self, tx: bool) -> civ::Result<()> {
        self.0.set_transmit(tx)
    }
    fn has_tuner(&self) -> bool {
        self.0.has_tuner()
    }
    fn has_meters(&self) -> bool {
        self.0.has_meters()
    }
    fn rest_needed(&mut self, keying: Duration) -> civ::Result<Duration> {
        self.0.rest_needed(keying)
    }
}

fn tx(segments: &[&str]) -> Transmission {
    Transmission {
        segments: segments.iter().map(|s| s.to_string()).collect(),
        read_ids: Vec::new(),
    }
}

#[test]
fn the_station_keys_a_handheld_without_tuning_or_reading_meters() {
    let (h, ptt, out) = rig();
    let mut st = Station::new(Scaled(h), station_cfg(), None);
    st.configure().unwrap();
    // A window start tunes nothing and keys nothing.
    st.start_window().unwrap();
    assert!(ptt.events().iter().all(|e| !e.1));
    st.transmit(&tx(&["R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K"]))
        .unwrap();
    // One PTT run per keyer piece, each released.
    let runs = out.played();
    assert_eq!(runs.len(), 2);
    assert_eq!(ptt.events().iter().filter(|e| e.1).count(), 2);
    assert!(!ptt.keyed());
    assert!(!st.tx_inhibited());
}

#[test]
fn the_station_watchdog_releases_a_handheld_stuck_on_a_hung_output() {
    let mut set = settings();
    set.playback_slack = Duration::from_secs(3600);
    set.max_run = Duration::from_secs(3600);
    let (h, ptt, out) = rig_with(set);
    out.set_hang(true);
    let mut st = Station::new(Scaled(h), station_cfg(), None);
    st.configure().unwrap();
    // The keying bound is about 1 s (scaled): the watchdog's 0.9 s or the
    // station's own stuck check, whichever comes first.
    let r = st.transmit(&tx(&["TEST"]));
    assert_eq!(r, Err(TxError::Stuck));
    assert!(!ptt.keyed());
    assert!(!st.tx_inhibited(), "released, so not inhibited");
}

#[test]
fn a_handheld_that_cannot_be_released_inhibits_transmitting() {
    let mut set = settings();
    set.max_run = Duration::from_secs(3600);
    let (h, ptt, _out) = rig_with(set);
    let mut st = Station::new(Scaled(h), station_cfg(), None);
    st.configure().unwrap();
    ptt.fail_releases(u32::MAX);
    assert_eq!(st.transmit(&tx(&["TEST"])), Err(TxError::Inhibited));
    assert!(st.tx_inhibited());
    ptt.fail_releases(0);
}

#[test]
fn the_duty_cycle_paces_a_long_transmission_on_receive() {
    let mut set = settings();
    // 0.5 s on the air (real) in any 1 s.
    set.duty = 0.5;
    set.duty_window = Duration::from_secs(1);
    let (h, ptt, _out) = rig_with(set);
    let mut st = Station::new(Scaled(h), station_cfg(), None);
    st.configure().unwrap();
    let t0 = Instant::now();
    st.transmit(&tx(&[
        "PART ONE OF A REPLY THAT GOES ON",
        "PART TWO OF A REPLY THAT GOES ON",
        "PART THREE OF A REPLY THAT ENDS",
    ]))
    .unwrap();
    let took = t0.elapsed();
    let ev = ptt.events();
    let on_air: Duration = ev
        .windows(2)
        .filter(|w| w[0].1 && !w[1].1)
        .map(|w| w[1].0 - w[0].0)
        .sum();
    // Six pieces of about 0.17 s each (real) do not fit in one window's 0.5 s,
    // so some waited.
    assert!(on_air > Duration::from_millis(700), "{on_air:?}");
    assert!(
        took > on_air + Duration::from_millis(300),
        "{took:?} {on_air:?}"
    );
    // Never more than the budget in any window.
    let budget = Duration::from_millis(500 + 30);
    let runs: Vec<(Instant, Instant)> = ev
        .windows(2)
        .filter(|w| w[0].1 && !w[1].1)
        .map(|w| (w[0].0, w[1].0))
        .collect();
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
fn a_storm_hold_keeps_a_handheld_off_the_air() {
    let (h, ptt, out) = rig();
    let mut st = Station::new(Scaled(h), station_cfg(), None);
    let hold = crate::storm::StormHold::new(Duration::from_secs(60));
    st.set_storm_hold(hold.clone());
    st.configure().unwrap();
    hold.set(Some("thunder forecast".into()));
    assert_eq!(
        st.transmit(&tx(&["TEST"])),
        Err(TxError::Storm("thunder forecast".into()))
    );
    assert!(out.played().is_empty());
    assert!(ptt.events().iter().all(|e| !e.1), "never keyed");
    hold.set(None);
    st.transmit(&tx(&["TEST"])).unwrap();
    assert_eq!(out.played().len(), 1);
    assert!(!ptt.keyed());
}
