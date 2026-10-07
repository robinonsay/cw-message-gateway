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
use serde::Deserialize;
use std::fmt;

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

/// Open the radio for `action`, a command that writes to it: the bring-up stage
/// must allow the command, checked before `open` is called, so that a refused
/// command opens no port and sends nothing; then the read-only preflight must pass
/// (with the radio's own Time-Out Timer required for `run`), or the radio is
/// closed again with nothing written. `open` opens the port, DTR and RTS lowered
/// ([`Ic7300::open`] for the real radio).
pub fn open_for<P: Port>(
    stage: Stage,
    action: Action,
    power_watts: u32,
    open: impl FnOnce() -> Result<Ic7300<P>>,
) -> Result<Ic7300<P>> {
    check(stage, action, power_watts)?;
    let mut rig = open()?;
    let report = civ::preflight::preflight(&mut rig, action == Action::Run);
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
        MockRadio::new(MockConfig {
            time_scale: 100.0,
            menu,
            ..MockConfig::default()
        })
    }

    /// [`open_for`] on `radio`; whether the port was opened.
    fn open(
        radio: &MockRadio,
        stage: Stage,
        action: Action,
        watts: u32,
    ) -> (Result<Ic7300<MockPort>>, bool) {
        let mut opened = false;
        let r = open_for(stage, action, watts, || {
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
        type Change = fn(&mut Menu);
        let cases: [(&str, Change); 4] = [
            ("USB SEND", |m| m.usb_send = 0x01),
            ("USB Keying (CW)", |m| m.usb_keying_cw = 0x02),
            ("USB Keying (RTTY)", |m| m.usb_keying_rtty = 0x01),
            ("Time-Out Timer (CI-V)", |m| m.time_out_timer = 0x00),
        ];
        for (name, change) in cases {
            let mut menu = Menu::default();
            change(&mut menu);
            let radio = mock(menu);
            // `run`: the Time-Out Timer is required too.
            let (r, opened) = open(&radio, Stage::Done, Action::Run, 40);
            let e = r
                .err()
                .unwrap_or_else(|| panic!("{name}: preflight passed"));
            assert!(opened && e.to_string().contains(name), "{name}: {e}");
            let cmds = radio.commands();
            assert!(cmds.iter().all(|(_, b)| is_read(b)), "{name}: {cmds:02X?}");
            assert!(radio.report().violations.is_empty());
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
    fn run_needs_the_radios_time_out_timer_and_the_bench_commands_do_not() {
        let menu = Menu {
            time_out_timer: 0x00,
            ..Menu::default()
        };
        let (r, _) = open(&mock(menu), Stage::Done, Action::Run, 40);
        assert!(r.is_err(), "run with the Time-Out Timer OFF");
        // `radio tune` and `radio cw` warn about it; the bench procedure stops on it.
        for action in [Action::Setup, Action::Tune, Action::Cw] {
            let (r, _) = open(&mock(menu), Stage::Done, action, 10);
            assert!(r.is_ok(), "{action:?}");
        }
        let (r, _) = open(&mock(Menu::default()), Stage::Done, Action::Run, 40);
        assert!(r.is_ok());
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
