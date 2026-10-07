use super::*;
use keyer_core::morse::Segments;

const SR: u32 = 8000;
const PITCH: f32 = 600.0;

fn settings() -> Settings {
    Settings {
        sample_rate: SR,
        pitch_hz: PITCH,
        min_level_dbfs: -65.0,
        scale: 1.0,
    }
}

/// A radio, its headphone audio and the sound card, in radio seconds.
#[derive(Clone)]
struct Radio {
    /// The box's key-down stretches, absolute.
    box_downs: Vec<(f64, f64)>,
    /// The key held down at the radio from this time on (a shorted optocoupler).
    stuck_from: Option<f64>,
    /// The key cable is out: the box's keying never reaches the radio.
    cable_out: bool,
    /// The radio's own keyer is set to iambic: a closed contact sends dots.
    paddle: bool,
    sidetone: f32,
    /// The sidetone's level swinging this many dB either way, this many times a
    /// second: (Hz, dB).
    wobble: Option<(f64, f32)>,
    /// Semi break-in hang after key-up; 0 is full break-in.
    hang: f64,
    /// Radio keying delay.
    delay: f64,
    /// Band noise (std dev) and the floor while muted or off.
    noise: f32,
    floor: f32,
    /// A carrier at the pitch on receive: (from, to, amplitude).
    carrier: Option<(f64, f64, f32)>,
    /// Stretches the sound card lost: only the floor arrives.
    dropouts: Vec<(f64, f64)>,
    /// The field station's CW at the pitch on receive: key-down stretches.
    field: Vec<(f64, f64)>,
    off: bool,
}

impl Default for Radio {
    fn default() -> Self {
        Self {
            box_downs: Vec::new(),
            stuck_from: None,
            cable_out: false,
            paddle: false,
            sidetone: 0.3,
            wobble: None,
            hang: 0.6,
            delay: 0.005,
            noise: 0.02,
            floor: 0.0002,
            carrier: None,
            dropouts: Vec::new(),
            field: Vec::new(),
            off: false,
        }
    }
}

fn inside(stretches: &[(f64, f64)], t: f64) -> bool {
    stretches.iter().any(|&(s, e)| t >= s && t < e)
}

impl Radio {
    /// Whether the key is closed at the radio's key jack at `t`.
    fn contact(&self, t: f64) -> bool {
        (!self.cable_out && inside(&self.box_downs, t)) || self.stuck_from.is_some_and(|s| t >= s)
    }

    /// Whether the radio keys its transmitter at `t`.
    fn keyed(&self, t: f64) -> bool {
        let t = t - self.delay;
        if !self.contact(t) {
            return false;
        }
        if self.paddle {
            // Dots at 20 wpm while the dot paddle is held.
            (t / 0.060).floor() as i64 % 2 == 0
        } else {
            true
        }
    }

    /// Whether the receiver is muted at `t`: keyed, or within the hang after.
    fn muted(&self, t: f64) -> bool {
        if self.keyed(t) {
            return true;
        }
        self.hang > 0.0
            && (1..=((self.hang / 0.005) as i64)).any(|k| self.keyed(t - k as f64 * 0.005))
    }

    fn sample(&self, t: f64, rng: &mut cw::synth::Noise) -> f32 {
        let mut one = [0.0f32];
        let w = 2.0 * std::f64::consts::PI * f64::from(PITCH) * t;
        if self.off || inside(&self.dropouts, t) {
            rng.add(&mut one, self.floor);
            return one[0];
        }
        let mut v = 0.0f32;
        if self.keyed(t) {
            let swing = self.wobble.map_or(1.0, |(hz, db)| {
                let s = (2.0 * std::f64::consts::PI * hz * t).sin() as f32;
                10.0f32.powf(db * s / 20.0)
            });
            v += self.sidetone * swing * w.sin() as f32;
        }
        if self.muted(t) {
            rng.add(&mut one, self.floor);
        } else {
            rng.add(&mut one, self.noise);
            if let Some((s, e, a)) = self.carrier {
                if t >= s && t < e {
                    v += a * w.sin() as f32;
                }
            }
            if inside(&self.field, t) {
                v += 0.2 * w.sin() as f32;
            }
        }
        v + one[0]
    }
}

/// Feeds the radio's audio from radio time `from` to `to` in 50 ms blocks, each
/// stamped `latency` (plus up to `jitter`, varying) after its last sample.
fn feed(
    m: &mut Monitor,
    t0: Instant,
    radio: &Radio,
    from: f64,
    to: f64,
    latency: f64,
    jitter: f64,
) {
    let mut rng = cw::synth::Noise::new(7);
    let block = SR as usize / 20;
    let mut t = from;
    let mut k = 0u32;
    while t < to {
        let samples: Vec<f32> = (0..block)
            .map(|i| radio.sample(t + i as f64 / f64::from(SR), &mut rng))
            .collect();
        t += block as f64 / f64::from(SR);
        k += 1;
        let extra = jitter * f64::from((k * 7) % 5) / 4.0;
        let at = t0 + Duration::from_secs_f64(t + latency + extra);
        m.push(at, &samples);
    }
}

/// `text` keyed at `wpm` from `start`: (segments, dot, absolute key-down stretches).
fn keyed(text: &str, wpm: u32, start: f64) -> (Segments, Duration, Vec<(f64, f64)>) {
    let segs = Segments::of(text.as_bytes()).unwrap();
    let dot = Duration::from_millis(keyer_core::morse::dot_ms(wpm).unwrap().into());
    let mut t = start;
    let mut downs = Vec::new();
    for s in segs.as_slice() {
        let len = f64::from(s.units) * dot.as_secs_f64();
        if s.down {
            downs.push((t, t + len));
        }
        t += len;
    }
    (segs, dot, downs)
}

struct Case {
    m: Monitor,
    t0: Instant,
    id: u64,
    radio: Radio,
    end: f64,
    /// Radio time of the last audio fed.
    fed: f64,
}

impl Case {
    fn feed_to(&mut self, to: f64) {
        feed(&mut self.m, self.t0, &self.radio, self.fed, to, 0.08, 0.04);
        self.fed = to;
    }

    /// The key's state just after the last audio arrived.
    fn state(&mut self) -> KeyState {
        self.m.key_state()
    }
}

/// A monitor that has heard `pre` s of band, then the box key `text` at 20 wpm,
/// then audio until `after` s past the run's end.
fn case(text: &str, tweak: impl FnOnce(&mut Radio), after: f64) -> Case {
    case_at(text, 20, tweak, after)
}

/// [`case`] at `wpm`.
fn case_at(text: &str, wpm: u32, tweak: impl FnOnce(&mut Radio), after: f64) -> Case {
    case_known(text, wpm, None, tweak, after)
}

/// [`case_at`], with `known` as the sidetone level `hfnode keyer sidetone` kept.
fn case_known(
    text: &str,
    wpm: u32,
    known: Option<f32>,
    tweak: impl FnOnce(&mut Radio),
    after: f64,
) -> Case {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    if let Some(db) = known {
        m.set_known_sidetone(db);
    }
    let start = 3.0;
    let (segs, dot, downs) = keyed(text, wpm, start);
    let end = downs.last().unwrap().1;
    let mut radio = Radio {
        box_downs: downs,
        ..Radio::default()
    };
    tweak(&mut radio);
    feed(&mut m, t0, &radio, 0.0, start, 0.08, 0.04);
    // As the rig does before keying.
    m.band(t0 + Duration::from_secs_f64(start + 0.1));
    let id = m.run_started(t0 + Duration::from_secs_f64(start), dot, segs.as_slice());
    feed(&mut m, t0, &radio, start, end + after, 0.08, 0.04);
    Case {
        m,
        t0,
        id,
        radio,
        end,
        fed: end + after,
    }
}

#[test]
fn a_run_keyed_at_the_radio_is_heard_at_its_delay() {
    let mut c = case("DE N0CALL K", |_| {}, 1.0);
    let j = c.m.judge(c.id).expect("audio covers the run");
    assert!(j.heard, "{j}");
    // The sound card's least delay (80 ms) and the radio's (5 ms).
    let lag = j.lag.as_millis() as i64;
    assert!((lag - 85).abs() <= 15, "{j}");
    assert!(j.contrast_db() > 30.0, "{j}");
    assert_eq!(c.m.sidetone_db(), Some(j.tone_db));
    assert_eq!(c.state(), KeyState::Open);
}

#[test]
fn full_break_in_is_heard_too() {
    let mut c = case("CQ CQ DE N0CALL", |r| r.hang = 0.0, 1.0);
    let j = c.m.judge(c.id).unwrap();
    assert!(j.heard, "{j}");
    assert_eq!(c.state(), KeyState::Open);
}

#[test]
fn a_single_dot_is_heard() {
    let mut c = case("E", |_| {}, 1.0);
    let j = c.m.judge(c.id).unwrap();
    assert!(j.heard, "{j}");
}

#[test]
fn no_judgement_before_the_audio_covers_the_run() {
    let mut c = case("DE N0CALL K", |_| {}, 0.2);
    assert_eq!(c.m.judge(c.id), None);
    // Nor can the audio show the key open yet.
    assert_eq!(c.state(), KeyState::Unsure);
    c.feed_to(c.end + 1.0);
    assert!(c.m.judge(c.id).unwrap().heard);
    assert_eq!(c.state(), KeyState::Open);
}

#[test]
fn faults_at_the_radio_are_not_heard() {
    type Fault = (&'static str, fn(&mut Radio));
    let faults: [Fault; 5] = [
        ("key cable out", |r| r.cable_out = true),
        ("sidetone off", |r| r.sidetone = 0.0),
        ("radio off", |r| r.off = true),
        ("paddle mode", |r| r.paddle = true),
        ("key stuck from the start", |r| r.stuck_from = Some(2.0)),
    ];
    for (name, f) in faults {
        let mut c = case("DE N0CALL K", f, 1.0);
        let j = c.m.judge(c.id).unwrap();
        assert!(!j.heard, "{name}: {j}");
        assert!(j.why_not().is_some(), "{name}");
    }
}

#[test]
fn a_strong_field_signal_during_the_run_is_not_taken_for_the_sidetone() {
    // The radio is not keyed (cable out) while the field station sends: its CW at
    // the pitch must not pass for the node's own keying.
    let mut c = case(
        "DE N0CALL K",
        |r| {
            r.cable_out = true;
            r.hang = 0.0;
            r.field = keyed("CQ CQ CQ TEST", 18, 2.9).2;
        },
        1.0,
    );
    assert!(!c.m.judge(c.id).unwrap().heard);
}

#[test]
fn a_key_stuck_during_the_run_is_caught_after_it() {
    let mut c = case(
        "DE N0CALL K",
        |r| r.stuck_from = Some(r.box_downs[8].0),
        0.3,
    );
    // Not yet: the delay and the 0.5 s.
    assert_eq!(c.state(), KeyState::Unsure);
    c.feed_to(c.end + 1.5);
    let KeyState::Held(why) = c.state() else {
        panic!("not held")
    };
    assert!(why.contains("after the box opened its key"), "{why}");
    // However late the audio stops.
    assert!(matches!(c.m.key_state(), KeyState::Held(_)));
}

#[test]
fn a_dropout_in_the_audio_does_not_clear_a_stuck_key() {
    // The key is closed at the radio from the ninth element on, and the sound card
    // loses a fifth of a second of audio shortly after the run: that silence looks
    // like the key opening, but the sidetone comes back, so the key is still held.
    let mut c = case(
        "DE N0CALL K",
        |r| {
            let end = r.box_downs.last().unwrap().1;
            r.stuck_from = Some(r.box_downs[8].0);
            r.dropouts = vec![(end + 0.3, end + 0.5)];
        },
        4.0,
    );
    let KeyState::Held(why) = c.state() else {
        panic!("not held: {:?}", c.state())
    };
    assert!(why.contains("after the box opened its key"), "{why}");
}

/// The default radio's sidetone, as `hfnode keyer sidetone` measures and keeps it.
fn bench_sidetone_db() -> f32 {
    let mut c = case("DE N0CALL K", |_| {}, 1.0);
    let j = c.m.judge(c.id).unwrap();
    assert!(j.heard, "{j}");
    j.tone_db
}

#[test]
fn a_key_held_with_the_band_just_under_the_sidetone_is_held() {
    // The safety audit's KB-2(ii) at run time: the sidetone measured and kept at the
    // bench, then the band only 7 or 9 dB under it while the box keys, and the key
    // held at the radio from the ninth element on. The band's level plus 10 dB is
    // over the sidetone, so only the sidetone's own level shows the key still closed.
    let known = bench_sidetone_db();
    for margin in [7.0f32, 9.0] {
        let mut c = case_known(
            "DE N0CALL K",
            20,
            Some(known),
            |r| {
                // A sine of amplitude a has the power of noise of standard deviation
                // a/sqrt 2.
                r.noise = r.sidetone / 2.0f32.sqrt() / 10.0f32.powf(margin / 20.0);
                r.stuck_from = Some(r.box_downs[8].0);
            },
            1.5,
        );
        assert!(!c.m.judge(c.id).unwrap().heard, "{margin} dB");
        let KeyState::Held(why) = c.state() else {
            panic!("{margin} dB: not held: {:?}", c.state())
        };
        assert!(why.contains("after the box opened its key"), "{why}");
    }
}

#[test]
fn a_look_before_the_run_is_judged_settles_nothing() {
    // The sidetone kept at the bench, then turned down: until the run is judged,
    // the level under which the key counts as open is a guess, here over (or at)
    // this quieter sidetone, so a key held at the radio from the last element on
    // looks open. Once the run is judged (heard, at its own level) the key is held:
    // a look before must not have settled the run as over, whichever way it read
    // it as open (a break in the tone, or too little of it).
    type Turned = (&'static str, f64, fn(&mut Radio));
    let known = bench_sidetone_db();
    let cases: [Turned; 2] = [
        // 14 dB down: a break under the guess.
        ("a break", 0.7, |r| r.sidetone *= 0.2),
        // 9 dB down, a little over the guess (10 dB under the kept level, with the
        // band too loud to set it), swinging 3 dB either way: never 50 ms under it,
        // but under it too often to be held.
        ("too little tone", 1.1, |r| {
            r.sidetone *= 10.0f32.powf(-9.0 / 20.0);
            r.wobble = Some((20.0, 3.0));
            r.noise = 0.025;
        }),
    ];
    for (name, look, turn) in cases {
        let mut c = case_known(
            "DE N0CALL N0CALL K",
            20,
            Some(known),
            |r| {
                turn(r);
                r.stuck_from = Some(r.box_downs.last().unwrap().0);
            },
            look,
        );
        assert_eq!(c.state(), KeyState::Open, "{name}: the guess");
        let j = c.m.judge(c.id).unwrap();
        assert!(j.heard, "{name}: {j}");
        // Too soon for the carrier check's second of tone (or, swinging, never
        // steady enough for it): only the judged run's level can show the key held.
        c.feed_to(c.end + look + 0.2);
        let KeyState::Held(why) = c.state() else {
            panic!("{name}: not held: {:?}", c.state())
        };
        assert!(why.contains("went on for"), "{name}: {why}");
    }
}

#[test]
fn a_key_stuck_from_the_first_element_is_caught_after_the_run() {
    let mut c = case(
        "DE N0CALL K",
        |r| r.stuck_from = Some(r.box_downs[0].0),
        1.5,
    );
    assert!(!c.m.judge(c.id).unwrap().heard);
    assert!(matches!(c.state(), KeyState::Held(_)));
}

#[test]
fn the_field_station_answering_at_once_is_not_a_stuck_key() {
    let mut c = case(
        "DE N0CALL K",
        |r| {
            r.hang = 0.0;
            let end = r.box_downs.last().unwrap().1;
            r.field = keyed("R R R TU", 20, end + 0.12).2;
        },
        3.0,
    );
    assert!(c.m.judge(c.id).unwrap().heard);
    assert_eq!(c.state(), KeyState::Open);
}

#[test]
fn a_carrier_answering_at_once_is_released_by_its_first_gap() {
    // Someone tunes up on the frequency right after the node's over: held, then
    // a break, then held again; the break shows the radio's key is open.
    let mut c = case(
        "DE N0CALL K",
        |r| {
            r.hang = 0.0;
            let end = r.box_downs.last().unwrap().1;
            r.carrier = Some((end + 0.6, end + 20.0, 0.3));
        },
        5.0,
    );
    assert_eq!(c.state(), KeyState::Open);
}

#[test]
fn a_steady_carrier_counts_only_after_thirty_seconds() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let radio = Radio {
        carrier: Some((5.0, 100.0, 0.3)),
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, 25.0, 0.08, 0.04);
    assert_eq!(m.key_state(), KeyState::Open);
    feed(&mut m, t0, &radio, 25.0, 36.0, 0.08, 0.04);
    let KeyState::Held(why) = m.key_state() else {
        panic!("not held")
    };
    assert!(why.contains("steady tone"), "{why}");
}

#[test]
fn a_weak_carrier_is_not_a_stuck_key() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let radio = Radio {
        carrier: Some((0.0, 100.0, 0.0005)),
        noise: 0.0,
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, 40.0, 0.08, 0.04);
    assert_eq!(m.key_state(), KeyState::Open);
}

#[test]
fn the_band_level_is_measured_on_receive_only() {
    let t0 = Instant::now();
    let at = |t: f64| t0 + Duration::from_secs_f64(t);
    let mut m = Monitor::starting_at(settings(), t0);
    let start = 3.0;
    let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
    let end = downs.last().unwrap().1;
    let radio = Radio {
        box_downs: downs,
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, start, 0.08, 0.04);
    let lv = m.band(at(start)).level_db.expect("level before the run");
    // Gaussian noise of std dev 0.02: -34 dBFS.
    assert!((lv + 34.0).abs() < 2.0, "{lv}");
    m.run_started(at(start), dot, segs.as_slice());
    feed(&mut m, t0, &radio, start, end + 1.0, 0.08, 0.04);
    // Kept while keying and just after: no receive audio since.
    let b = m.band(at(end + 1.1));
    assert!(b.audio);
    assert_eq!(b.level_db, Some(lv));
    // Long after, measured afresh.
    feed(&mut m, t0, &radio, end + 1.0, end + 5.0, 0.08, 0.04);
    let lv2 = m.band(at(end + 5.1)).level_db.unwrap();
    assert!((lv2 + 34.0).abs() < 2.0, "{lv2}");
    assert_ne!(lv2, lv);
    // No audio for a while; the level stands for its two minutes.
    let b = m.band(at(end + 7.0));
    assert!(!b.audio);
    assert_eq!(b.level_db, Some(lv2));
    assert_eq!(m.band(at(end + 200.0)).level_db, None);
}

#[test]
fn a_radio_switched_off_reads_below_the_minimum_level() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let radio = Radio {
        off: true,
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, 3.0, 0.08, 0.0);
    let lv = m.band(t0 + Duration::from_secs(3)).level_db.unwrap();
    assert!(lv < -65.0, "{lv}");
}

#[test]
fn a_run_stopped_early_is_judged_up_to_the_stop() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let start = 3.0;
    let (segs, dot, downs) = keyed("DE N0CALL N0CALL K", 20, start);
    // The box was stopped in the middle of the 10th element's gap.
    let stop = (downs[9].1 + downs[10].0) / 2.0;
    let radio = Radio {
        box_downs: downs.into_iter().filter(|&(s, _)| s < stop).collect(),
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, start, 0.08, 0.04);
    let id = m.run_started(t0 + Duration::from_secs_f64(start), dot, segs.as_slice());
    m.key_opened(id, t0 + Duration::from_secs_f64(stop));
    feed(&mut m, t0, &radio, start, stop + 1.5, 0.08, 0.04);
    let j = m.judge(id).expect("covered up to the stop");
    assert!(j.heard, "{j}");
    assert_eq!(m.key_state(), KeyState::Open);
}

#[test]
fn lost_audio_restarts_the_timing() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let radio = Radio::default();
    feed(&mut m, t0, &radio, 0.0, 2.0, 0.08, 0.0);
    // Four seconds of audio never arrive; then it carries on.
    let start = 7.0;
    let (segs, dot, downs) = keyed("TEST", 20, start);
    let radio = Radio {
        box_downs: downs.clone(),
        ..Radio::default()
    };
    let mut rng = cw::synth::Noise::new(3);
    let block = SR as usize / 20;
    let mut t = 6.0;
    while t < downs.last().unwrap().1 + 1.5 {
        let samples: Vec<f32> = (0..block)
            .map(|i| radio.sample(t + i as f64 / f64::from(SR), &mut rng))
            .collect();
        t += block as f64 / f64::from(SR);
        m.push(t0 + Duration::from_secs_f64(t + 0.08), &samples);
    }
    let id = m.run_started(t0 + Duration::from_secs_f64(start), dot, segs.as_slice());
    assert!(m.judge(id).unwrap().heard);
}

#[test]
fn a_key_closed_at_the_radio_just_before_a_run_is_held_after_it() {
    // Closed half a second before the box keys: not yet a carrier, so the run is
    // keyed; the key counts as open only once the tone drops near the band's level
    // from before, not near the tone just before the run.
    let t0 = Instant::now();
    let at = |t: f64| t0 + Duration::from_secs_f64(t);
    let mut m = Monitor::starting_at(settings(), t0);
    let start = 4.0;
    let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
    let end = downs.last().unwrap().1;
    let radio = Radio {
        box_downs: downs,
        stuck_from: Some(start - 0.5),
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, 3.0, 0.08, 0.04);
    m.band(at(3.0));
    feed(&mut m, t0, &radio, 3.0, start, 0.08, 0.04);
    let b = m.band(at(start));
    assert_eq!(b.carrier_db, None);
    let lv = b.level_db.expect("the band, from before the key closed");
    assert!((lv + 34.0).abs() < 2.0, "{lv}");
    let id = m.run_started(at(start), dot, segs.as_slice());
    feed(&mut m, t0, &radio, start, end + 1.5, 0.08, 0.04);
    assert!(!m.judge(id).unwrap().heard);
    let KeyState::Held(why) = m.key_state() else {
        panic!("not held")
    };
    assert!(why.contains("after the box opened its key"), "{why}");
}

#[test]
fn a_steady_tone_now_is_a_carrier_and_not_the_band() {
    type Tone = (&'static str, fn(&mut Radio));
    let tones: [Tone; 2] = [
        ("a carrier on the frequency", |r| {
            r.carrier = Some((2.0, 100.0, 0.3))
        }),
        ("the key closed at the radio", |r| r.stuck_from = Some(2.0)),
    ];
    for (name, f) in tones {
        let t0 = Instant::now();
        let at = |t: f64| t0 + Duration::from_secs_f64(t);
        let mut m = Monitor::starting_at(settings(), t0);
        let mut radio = Radio::default();
        f(&mut radio);
        feed(&mut m, t0, &radio, 0.0, 1.8, 0.08, 0.04);
        let b = m.band(at(1.8));
        assert_eq!(b.carrier_db, None, "{name}");
        let lv = b.level_db.unwrap();
        feed(&mut m, t0, &radio, 1.8, 4.0, 0.08, 0.04);
        let b = m.band(at(4.0));
        let db = b.carrier_db.unwrap_or_else(|| panic!("{name}: no carrier"));
        assert!((db + 13.5).abs() < 2.0, "{name}: {db}");
        // The band's level from before it stands.
        assert_eq!(b.level_db, Some(lv), "{name}");
    }
    // Held from the start: the tone is never taken for the band.
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let radio = Radio {
        stuck_from: Some(0.0),
        ..Radio::default()
    };
    feed(&mut m, t0, &radio, 0.0, 3.0, 0.08, 0.04);
    let b = m.band(t0 + Duration::from_secs(3));
    assert!(b.carrier_db.is_some());
    assert_eq!(b.level_db, None);
}

/// Band noise through a narrow CW filter at the pitch: mostly at the pitch, but
/// its level jumps about.
fn narrow_noise(m: &mut Monitor, t0: Instant, secs: f64, std: f32) {
    let mut rng = cw::synth::Noise::new(11);
    let sr = f64::from(SR);
    // A two-pole resonator at the pitch, about 50 Hz wide.
    let r = (-std::f64::consts::PI * 50.0 / sr).exp();
    let w = 2.0 * std::f64::consts::PI * f64::from(PITCH) / sr;
    let (a1, a2) = (2.0 * r * w.cos(), -r * r);
    let gain = (1.0 - r * r).sqrt() * 2.0;
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    let block = SR as usize / 20;
    let mut t = 0.0;
    while t < secs {
        let mut x = vec![0.0f32; block];
        rng.add(&mut x, std);
        let samples: Vec<f32> = x
            .iter()
            .map(|&v| {
                let y = gain * f64::from(v) + a1 * y1 + a2 * y2;
                (y2, y1) = (y1, y);
                y as f32
            })
            .collect();
        t += block as f64 / sr;
        m.push(t0 + Duration::from_secs_f64(t + 0.08), &samples);
    }
}

#[test]
fn band_noise_through_a_narrow_filter_is_not_a_carrier() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    narrow_noise(&mut m, t0, 40.0, 0.02);
    let b = m.band(t0 + Duration::from_secs(40));
    assert_eq!(b.carrier_db, None);
    let lv = b.level_db.unwrap();
    assert!(lv > -50.0, "{lv}");
    assert_eq!(m.key_state(), KeyState::Open);
}

#[test]
fn semi_break_in_is_heard_over_a_loud_band() {
    // The receiver is muted in the run's gaps and for the hang after it, so the
    // band before the run is louder than the gaps: it is scored on its own.
    for (text, wpm) in [("TU K", 20), ("R 42 K", 30), ("E", 20)] {
        let mut c = case_at(text, wpm, |r| r.noise = 0.028, 1.0);
        let j = c.m.judge(c.id).unwrap();
        assert!(j.heard, "{text} at {wpm} wpm: {j}");
        assert_eq!(c.state(), KeyState::Open, "{text}");
    }
}

#[test]
fn a_little_lost_audio_keeps_the_timing() {
    for lost in [0.1, 0.3, 0.6, 0.9] {
        let t0 = Instant::now();
        let mut m = Monitor::starting_at(settings(), t0);
        let start = 4.0;
        let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
        let end = downs.last().unwrap().1;
        let radio = Radio {
            box_downs: downs,
            ..Radio::default()
        };
        feed(&mut m, t0, &radio, 0.0, 2.0, 0.08, 0.04);
        // `lost` s of audio never arrive.
        feed(&mut m, t0, &radio, 2.0 + lost, start, 0.08, 0.04);
        m.band(t0 + Duration::from_secs_f64(start));
        let id = m.run_started(t0 + Duration::from_secs_f64(start), dot, segs.as_slice());
        feed(&mut m, t0, &radio, start, end + 1.5, 0.08, 0.04);
        let j = m.judge(id).unwrap();
        assert!(j.heard, "{lost} s lost: {j}");
        let lag = j.lag.as_millis() as i64;
        assert!((lag - 85).abs() <= 15, "{lost} s lost: {j}");
        assert_eq!(m.key_state(), KeyState::Open, "{lost} s lost");
    }
}

#[test]
fn audio_held_up_then_caught_up_is_not_lost_audio() {
    // The capture stalls for 0.9 s early in a run, then catches up at 1.25 times
    // real time: every sample arrives, late at first.
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    let start = 3.0;
    let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
    let end = downs.last().unwrap().1;
    let radio = Radio {
        box_downs: downs,
        ..Radio::default()
    };
    let mut rng = cw::synth::Noise::new(7);
    let block = SR as usize / 20;
    let at = |s: f64| t0 + Duration::from_secs_f64(s);
    let (stall, resume) = (start + 0.3, start + 1.2);
    let (mut t, mut id, mut last) = (0.0, None, 0.0f64);
    while t < end + 1.5 {
        if id.is_none() && t >= start {
            m.band(at(start));
            id = Some(m.run_started(at(start), dot, segs.as_slice()));
        }
        let samples: Vec<f32> = (0..block)
            .map(|i| radio.sample(t + i as f64 / f64::from(SR), &mut rng))
            .collect();
        t += block as f64 / f64::from(SR);
        let due = t + 0.08;
        last = if t <= stall {
            due
        } else {
            due.max(resume + 0.08).max(last + 0.04)
        };
        m.push(at(last), &samples);
    }
    let j = m.judge(id.unwrap()).unwrap();
    assert!(j.heard, "{j}");
    let lag = j.lag.as_millis() as i64;
    assert!((lag - 85).abs() <= 15, "{j}");
    assert_eq!(m.key_state(), KeyState::Open);
}

#[test]
fn audio_held_up_by_a_busy_computer_is_not_lost_audio() {
    // At 20x real time, the few milliseconds a busy computer holds up the audio
    // are a tenth of a second of radio time: not audio lost, so the timing stays.
    let scale = 20.0;
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(
        Settings {
            scale: scale as f32,
            ..settings()
        },
        t0,
    );
    let at = |t: f64, late: f64| t0 + Duration::from_secs_f64(t / scale + late);
    let start = 4.0;
    let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
    let end = downs.last().unwrap().1;
    let radio = Radio {
        box_downs: downs,
        ..Radio::default()
    };
    let mut rng = cw::synth::Noise::new(7);
    let block = SR as usize / 20;
    let (mut t, mut id) = (0.0, None);
    while t < end + 1.5 {
        if id.is_none() && t >= start {
            m.band(at(start, 0.004));
            id = Some(m.run_started(at(start, 0.0), dot, segs.as_slice()));
        }
        let samples: Vec<f32> = (0..block)
            .map(|i| radio.sample(t + i as f64 / f64::from(SR), &mut rng))
            .collect();
        t += block as f64 / f64::from(SR);
        // 4 ms on the way; 5 ms more from a second before the run to its end.
        let late = if (start - 1.0..end).contains(&t) {
            0.009
        } else {
            0.004
        };
        m.push(at(t, late), &samples);
    }
    let j = m.judge(id.unwrap()).unwrap();
    assert!(j.heard, "{j}");
    let lag = j.lag.as_millis() as i64;
    assert!((lag - 85).abs() <= 15, "{j}");
    assert_eq!(m.key_state(), KeyState::Open);
}

#[test]
fn without_audio_after_a_run_the_key_is_not_shown_open() {
    // The sound card drops out as the run ends: the box's word that its key is open
    // does not show the radio's.
    let mut c = case("DE N0CALL K", |_| {}, 0.3);
    assert_eq!(c.state(), KeyState::Unsure);
    assert_eq!(c.m.key_state(), KeyState::Unsure);
}

#[test]
fn the_sidetone_pitch_and_the_longest_tone_are_measured() {
    let t0 = Instant::now();
    let mut m = Monitor::starting_at(settings(), t0);
    m.record(true);
    let start = 3.0;
    let (segs, dot, downs) = keyed("DE N0CALL K", 20, start);
    let end = downs.last().unwrap().1;
    // The radio's sidetone is at 640 Hz, not the 600 Hz set: still heard, and the
    // pitch measured says so.
    let mut radio = Radio {
        box_downs: downs,
        ..Radio::default()
    };
    radio.carrier = None;
    let pitch = 640.0;
    let mut rng = cw::synth::Noise::new(5);
    let block = SR as usize / 20;
    let mut t = 0.0;
    let mut id = None;
    while t < end + 1.5 {
        if id.is_none() && t >= start {
            id = Some(m.run_started(t0 + Duration::from_secs_f64(start), dot, segs.as_slice()));
        }
        let samples: Vec<f32> = (0..block)
            .map(|i| {
                let at = t + i as f64 / f64::from(SR);
                let mut one = [0.0f32];
                rng.add(&mut one, 0.0005);
                let tone = if radio.keyed(at) {
                    0.3 * (2.0 * std::f64::consts::PI * pitch * at).sin() as f32
                } else {
                    0.0
                };
                one[0] + tone
            })
            .collect();
        t += block as f64 / f64::from(SR);
        m.push(t0 + Duration::from_secs_f64(t + 0.05), &samples);
    }
    let id = id.unwrap();
    assert!(m.judge(id).unwrap().heard);
    let p = m.pitch(id).unwrap();
    assert!((p - 640.0).abs() <= 5.0, "{p}");
    // The longest element is a dash: 180 ms.
    let longest = m.longest_tone(t0).as_millis() as i64;
    assert!((longest - 180).abs() <= 20, "{longest} ms");
}
