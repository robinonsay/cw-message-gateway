//! Bring-up stages for a real radio, and what each one allows.
//!
//! `station.commissioned` in the config names the last bring-up stage of
//! `docs/hardware-test-plan.md` that has passed on this radio. Commands that need a
//! later stage are refused, so a stage cannot be skipped by running the wrong
//! command or by starting the service early, and the power stays at bench level
//! until keying has been proven at that level.
//!
//! Every command that writes settings or can transmit (`radio setup`, `radio tune`,
//! `radio cw` and `run`) opens the radio through [`open_for`]: the stage is checked
//! before the port is opened, and the read-only preflight ([`civ::preflight`]) must
//! pass before anything is written, whatever the stage. `radio rx` is the exception:
//! it stops the keyer and switches to receive (`17 FF`, `1C 00 00`) without a
//! preflight, so that it can unkey a radio the preflight would refuse.

use anyhow::{bail, Result};
use civ::ic7300::{Ic7300, Port};
use civ::preflight::Purpose;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Nothing has passed: only commands that never write a setting
    /// (`radio check`, `radio status`, `listen`) and `radio rx`.
    #[default]
    None,
    /// The read-only preflight passed and every value matched the radio's own
    /// screens. Allows `radio setup`, which writes settings but never transmits.
    Link,
    /// `radio setup` was read back correctly and the display matched. Allows
    /// `radio tune` into a dummy load.
    Setup,
    /// The tuner cycled into a dummy load and the radio came back to receive.
    /// Allows `radio cw` at bench power.
    Tune,
    /// Short CW, the SWR reading, the high-SWR lockout and the watchdog all passed at
    /// bench power. Allows power above [`BENCH_MAX_WATTS`].
    Keying,
    /// Power calibration and the hardware transmit timer passed. Allows `run`.
    Done,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Link => "link",
            Self::Setup => "setup",
            Self::Tune => "tune",
            Self::Keying => "keying",
            Self::Done => "done",
        })
    }
}

/// Highest `station.power_watts` allowed before [`Stage::Keying`].
pub const BENCH_MAX_WATTS: u32 = 10;

/// Commands that write to the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Setup,
    Tune,
    Cw,
    Run,
}

impl Action {
    fn needs(self) -> Stage {
        match self {
            Self::Setup => Stage::Link,
            Self::Tune => Stage::Setup,
            Self::Cw => Stage::Tune,
            Self::Run => Stage::Done,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Setup => "radio setup",
            Self::Tune => "radio tune",
            Self::Cw => "radio cw",
            Self::Run => "run",
        }
    }

    /// What the command will do with the radio, for the preflight: `radio setup`
    /// writes settings and never transmits; the others can transmit.
    pub fn purpose(self) -> Purpose {
        match self {
            Self::Setup => Purpose::Setup,
            Self::Tune | Self::Cw | Self::Run => Purpose::Transmit,
        }
    }
}

/// Whether `action` may run at `stage` with `power_watts` configured.
pub fn check(stage: Stage, action: Action, power_watts: u32) -> Result<()> {
    let needs = action.needs();
    if stage < needs {
        bail!(
            "`{}` needs bring-up stage `{needs}` to have passed, but station.commissioned \
             is `{stage}` (docs/hardware-test-plan.md, \"Bring-up stages\")",
            action.name()
        );
    }
    if power_watts > BENCH_MAX_WATTS && stage < Stage::Keying {
        bail!(
            "station.power_watts is {power_watts}: above {BENCH_MAX_WATTS} W needs bring-up \
             stage `keying` to have passed, but station.commissioned is `{stage}` \
             (docs/hardware-test-plan.md, \"Bring-up stages\")"
        );
    }
    Ok(())
}

/// Most `station.max_key_seconds` that `run` accepts: below the hardware transmit
/// timer's 60 s (docs/hardware-test-plan.md, step 10), so that the node's own
/// watchdog acts first and the timer stays the backstop. The bench may set more,
/// to test that timer.
pub const RUN_MAX_KEY_SECONDS: u64 = 55;

/// For `run`: `max_key_seconds` below [`RUN_MAX_KEY_SECONDS`], and enough for one
/// character at `key_speed_wpm` ([`crate::station::shortest_max_key`]), so that
/// every piece can be keyed whole under the watchdog; and the longest reply the
/// session builds ([`protocol::MAX_CHUNKS`] chunks of `chunk_chars`) within
/// [`crate::station::MAX_TRANSMISSION`] at that speed, counting 10 Morse units a
/// character (PARIS is 50 with its word gap), or the station would refuse it.
/// Other commands take any value the config allows: the bench sets these low, or
/// high, on purpose to test the watchdog and the hardware timer.
pub fn check_keying(action: Action, l: Limits) -> Result<()> {
    if action != Action::Run {
        return Ok(());
    }
    let (secs, wpm) = (l.max_key_seconds, l.key_speed_wpm);
    if secs > RUN_MAX_KEY_SECONDS {
        bail!(
            "station.max_key_seconds is {secs}: `run` needs {RUN_MAX_KEY_SECONDS} or less, \
             below the hardware transmit timer (docs/hardware-test-plan.md, step 10)"
        );
    }
    let least = crate::station::shortest_max_key(wpm).as_secs_f32().ceil() as u64;
    if secs < least {
        bail!(
            "station.max_key_seconds is {secs}: at {wpm} wpm `run` needs at least {least}, \
             or one character keyed would outlast the watchdog"
        );
    }
    let dot = Duration::from_millis(1200) / wpm.max(1);
    let longest = dot * (protocol::MAX_CHUNKS * l.chunk_chars * 10) as u32;
    let limit = crate::station::MAX_TRANSMISSION;
    if longest > limit {
        bail!(
            "station.chunk_chars {} at {wpm} wpm: the longest reply ({} chunks) would key for \
             about {:.0} min, more than the {:.0} min one transmission may take; lower \
             chunk_chars or raise key_speed_wpm",
            l.chunk_chars,
            protocol::MAX_CHUNKS,
            longest.as_secs_f32() / 60.0,
            limit.as_secs_f32() / 60.0
        );
    }
    Ok(())
}

/// The `[station]` settings [`open_for`] checks before it opens the port.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub power_watts: u32,
    pub max_key_seconds: u64,
    pub key_speed_wpm: u32,
    pub chunk_chars: usize,
}

impl Limits {
    pub fn of(s: &crate::config::Station) -> Self {
        Self {
            power_watts: s.power_watts,
            max_key_seconds: s.max_key_seconds,
            key_speed_wpm: s.key_speed_wpm,
            chunk_chars: s.chunk_chars,
        }
    }
}

/// Open the radio for `action`, a command that writes to it: the bring-up stage
/// and, for `run`, the keying limit ([`check_keying`]) must allow the command,
/// checked before `open` is called, so that a refused
/// command opens no port and sends nothing; then the read-only preflight must pass
/// (for a command that can transmit, with the radio's own Time-Out Timer at 3 min
/// and its TX Inhibit OFF), or the radio is closed again with nothing written.
/// `open` opens the port, DTR and RTS lowered ([`Ic7300::open`] for the real radio).
pub fn open_for<P: Port>(
    stage: Stage,
    action: Action,
    limits: Limits,
    open: impl FnOnce() -> Result<Ic7300<P>>,
) -> Result<Ic7300<P>> {
    check(stage, action, limits.power_watts)?;
    check_keying(action, limits)?;
    let mut rig = open()?;
    let report = civ::preflight::preflight(&mut rig, action.purpose());
    for line in report.to_string().lines() {
        log::info!("preflight: {line}");
    }
    if !report.passed() {
        let failed: Vec<&str> = report.failures().map(|c| c.name).collect();
        bail!(
            "radio preflight failed ({}); nothing was written to the radio",
            failed.join(", ")
        );
    }
    Ok(rig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use civ::mock::{is_read, Menu, MockConfig, MockPort, MockRadio};

    #[test]
    fn each_command_needs_the_stage_before_it() {
        use Action::*;
        let stages = [
            Stage::None,
            Stage::Link,
            Stage::Setup,
            Stage::Tune,
            Stage::Keying,
            Stage::Done,
        ];
        for (action, first) in [(Setup, 1), (Tune, 2), (Cw, 3), (Run, 5)] {
            for (i, &stage) in stages.iter().enumerate() {
                assert_eq!(
                    check(stage, action, 10).is_ok(),
                    i >= first,
                    "{action:?} at {stage}"
                );
            }
        }
    }

    #[test]
    fn run_keeps_the_watchdog_below_the_hardware_timer_and_above_one_character() {
        // The safety audit's K14: up to 120 s was accepted, and nothing checked that
        // one character fits under the watchdog.
        let limits = |max_key_seconds, key_speed_wpm, chunk_chars| Limits {
            power_watts: 40,
            max_key_seconds,
            key_speed_wpm,
            chunk_chars,
        };
        for (secs, wpm, ok) in [
            (45, 18, true),
            (55, 18, true),
            (56, 18, false),
            (120, 18, false),
            (5, 18, true),
            (4, 18, false),
            (8, 6, false),
            (9, 6, true),
        ] {
            assert_eq!(
                check_keying(Action::Run, limits(secs, wpm, 30)).is_ok(),
                ok,
                "{secs} s at {wpm} wpm"
            );
        }
        // The longest reply within the 30 minutes of one transmission (K13): 26
        // chunks of 60 characters at 11 wpm or faster.
        assert!(check_keying(Action::Run, limits(45, 11, 60)).is_ok());
        assert!(check_keying(Action::Run, limits(45, 10, 60)).is_err());
        assert!(check_keying(Action::Run, limits(45, 6, 34)).is_ok());
        assert!(check_keying(Action::Run, limits(45, 6, 35)).is_err());
        // The bench tests the watchdog and the hardware timer with values outside.
        for action in [Action::Setup, Action::Tune, Action::Cw] {
            assert!(check_keying(action, limits(120, 6, 60)).is_ok());
            assert!(check_keying(action, limits(1, 6, 60)).is_ok());
        }
        // Refused before the port is opened.
        let radio = MockRadio::new(MockConfig::default());
        let limits = limits(60, 18, 60);
        let mut opened = false;
        let Err(e) = open_for(Stage::Done, Action::Run, limits, || {
            opened = true;
            Ok(Ic7300::with_port(radio.port(), 0x94))
        }) else {
            panic!("run opened with max_key_seconds 60");
        };
        assert!(e.to_string().contains("hardware transmit timer"), "{e}");
        assert!(!opened);
    }

    #[test]
    fn power_stays_at_bench_level_until_keying_has_passed() {
        assert!(check(Stage::Tune, Action::Cw, 10).is_ok());
        assert!(check(Stage::Tune, Action::Cw, 11).is_err());
        assert!(check(Stage::Link, Action::Setup, 40).is_err());
        assert!(check(Stage::Keying, Action::Cw, 50).is_ok());
        assert!(check(Stage::Done, Action::Run, 40).is_ok());
    }

    #[test]
    fn stage_names_parse() {
        #[derive(Deserialize)]
        struct T {
            s: Stage,
        }
        for (name, stage) in [
            ("none", Stage::None),
            ("keying", Stage::Keying),
            ("done", Stage::Done),
        ] {
            let t: T = toml::from_str(&format!("s = \"{name}\"")).unwrap();
            assert_eq!(t.s, stage);
            assert_eq!(stage.to_string(), name);
        }
        assert!(toml::from_str::<T>("s = \"all\"").is_err());
    }

    const STAGES: [Stage; 6] = [
        Stage::None,
        Stage::Link,
        Stage::Setup,
        Stage::Tune,
        Stage::Keying,
        Stage::Done,
    ];
    const ACTIONS: [Action; 4] = [Action::Setup, Action::Tune, Action::Cw, Action::Run];

    fn mock(menu: Menu) -> MockRadio {
        mock_with(|c| c.menu = menu)
    }

    fn mock_with(change: impl FnOnce(&mut MockConfig)) -> MockRadio {
        let mut cfg = MockConfig {
            time_scale: 100.0,
            ..MockConfig::default()
        };
        change(&mut cfg);
        MockRadio::new(cfg)
    }

    /// [`open_for`] on `radio`; whether the port was opened.
    fn open(
        radio: &MockRadio,
        stage: Stage,
        action: Action,
        watts: u32,
    ) -> (Result<Ic7300<MockPort>>, bool) {
        let mut opened = false;
        let limits = Limits {
            power_watts: watts,
            max_key_seconds: 45,
            key_speed_wpm: 18,
            chunk_chars: 60,
        };
        let r = open_for(stage, action, limits, || {
            opened = true;
            Ok(Ic7300::with_port(radio.port(), 0x94))
        });
        (r, opened)
    }

    #[test]
    fn a_refused_stage_or_power_opens_nothing_and_sends_nothing() {
        for stage in STAGES {
            for action in ACTIONS {
                for watts in [5, BENCH_MAX_WATTS, BENCH_MAX_WATTS + 1, 100] {
                    let radio = mock(Menu::default());
                    let refused = check(stage, action, watts).is_err();
                    let (r, opened) = open(&radio, stage, action, watts);
                    let case = format!("{action:?} at {stage}, {watts} W");
                    assert_eq!(r.is_err(), refused, "{case}");
                    assert_eq!(opened, !refused, "{case}");
                    if refused {
                        assert!(radio.commands().is_empty(), "{case}: bytes sent");
                    } else {
                        // Allowed: the preflight's reads went out, and nothing else.
                        let cmds = radio.commands();
                        assert!(!cmds.is_empty(), "{case}");
                        assert!(cmds.iter().all(|(_, b)| is_read(b)), "{case}: {cmds:02X?}");
                    }
                }
            }
        }
    }

    #[test]
    fn nothing_is_written_after_a_failed_preflight() {
        type Change = fn(&mut MockConfig);
        let cases: [(&str, Change); 12] = [
            ("USB SEND", |c| c.menu.usb_send = 0x01),
            ("USB Keying (CW)", |c| c.menu.usb_keying_cw = 0x02),
            ("USB Keying (RTTY)", |c| c.menu.usb_keying_rtty = 0x01),
            ("Time-Out Timer (CI-V)", |c| c.menu.time_out_timer = 0x00),
            ("Time-Out Timer (CI-V)", |c| c.menu.time_out_timer = 0x05),
            ("VOX", |c| c.menu.vox = 0x01),
            ("PTT Start (tuner)", |c| c.menu.ptt_tune = 0x01),
            ("TX Inhibit", |c| c.tx_inhibit = true),
            ("CI-V USB port", |c| c.menu.civ_usb_port = 0x00),
            ("keyer dot/dash ratio", |c| c.menu.keyer_ratio = 0x33),
            ("scope data output", |c| c.menu.scope_data_output = 0x01),
            ("USB SEND", |c| c.menu.usb_send = 0x02),
        ];
        for (name, change) in cases {
            // Every command that can transmit; the stage allows each at 10 W.
            for action in [Action::Tune, Action::Cw, Action::Run] {
                let radio = mock_with(change);
                let (r, opened) = open(&radio, Stage::Done, action, 10);
                let e = r
                    .err()
                    .unwrap_or_else(|| panic!("{name}, {action:?}: preflight passed"));
                assert!(opened && e.to_string().contains(name), "{name}: {e}");
                let cmds = radio.commands();
                assert!(cmds.iter().all(|(_, b)| is_read(b)), "{name}: {cmds:02X?}");
                let rep = radio.report();
                assert!(rep.violations.is_empty() && rep.keyed.is_empty() && rep.tunes == 0);
            }
        }
        // Another radio, or nothing, at the address: refused at the first read.
        let radio = MockRadio::new(MockConfig {
            transceiver_id: 0xB6,
            ..MockConfig::default()
        });
        let (r, _) = open(&radio, Stage::Done, Action::Run, 40);
        assert!(r.is_err());
        assert_eq!(radio.commands().len(), 1);
    }

    #[test]
    fn every_command_that_can_transmit_needs_the_time_out_timer_at_3_min() {
        // OFF, 5, 10, 20 and 30 min.
        for tot in [0x00, 0x02, 0x03, 0x04, 0x05] {
            let menu = Menu {
                time_out_timer: tot,
                ..Menu::default()
            };
            for action in [Action::Tune, Action::Cw, Action::Run] {
                let radio = mock(menu);
                let (r, opened) = open(&radio, Stage::Done, action, 10);
                let e = r.err().unwrap_or_else(|| panic!("{action:?} at {tot:02X}"));
                assert!(opened && e.to_string().contains("Time-Out Timer"), "{e}");
                assert!(radio.commands().iter().all(|(_, b)| is_read(b)));
            }
            // `radio setup` never transmits: it only warns.
            let (r, _) = open(&mock(menu), Stage::Done, Action::Setup, 10);
            assert!(r.is_ok(), "setup at {tot:02X}");
        }
        for action in ACTIONS {
            let (r, _) = open(&mock(Menu::default()), Stage::Done, action, 10);
            assert!(r.is_ok(), "{action:?} at 3 min");
        }
    }

    #[test]
    fn tx_inhibit_on_refuses_every_command_that_can_transmit_but_setup() {
        for action in [Action::Tune, Action::Cw, Action::Run] {
            let radio = mock_with(|c| c.tx_inhibit = true);
            let (r, _) = open(&radio, Stage::Done, action, 10);
            assert!(r.is_err(), "{action:?}");
            assert!(radio.commands().iter().all(|(_, b)| is_read(b)));
        }
        // `radio setup` is how a person clears it: only a warning.
        let radio = mock_with(|c| c.tx_inhibit = true);
        assert!(open(&radio, Stage::Done, Action::Setup, 10).0.is_ok());
    }

    #[test]
    fn the_first_write_after_the_preflight_puts_the_radio_on_receive() {
        let radio = mock(Menu::default());
        let rig = open(&radio, Stage::Tune, Action::Cw, 10).0.unwrap();
        let reads = radio.commands().len();
        let cfg: crate::config::Config =
            toml::from_str(include_str!("../../../hfnode.example.toml")).unwrap();
        let st = crate::station::Station::new(
            rig,
            crate::station::StationConfig::from_config(&cfg.station),
            None,
        );
        st.configure().unwrap();
        let cmds = radio.commands();
        assert!(cmds[..reads].iter().all(|(_, b)| is_read(b)));
        assert_eq!(cmds[reads].1, [0x1C, 0x00, 0x00], "{cmds:02X?}");
    }
}
