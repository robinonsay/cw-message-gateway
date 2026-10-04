//! Bring-up stages for a real radio, and what each one allows.
//!
//! `station.commissioned` in the config names the last stage of
//! `docs/first-contact.md` that has passed on this radio. Commands that need a later
//! stage are refused, so a stage cannot be skipped by running the wrong command or
//! by starting the service early, and the power stays at bench level until keying
//! has been proven at that level.
//!
//! Every command that writes to the radio also runs the read-only preflight first
//! ([`civ::preflight`]), whatever the stage.

use anyhow::{bail, Result};
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
             is `{stage}` (docs/first-contact.md)",
            action.name()
        );
    }
    if power_watts > BENCH_MAX_WATTS && stage < Stage::Keying {
        bail!(
            "station.power_watts is {power_watts}: above {BENCH_MAX_WATTS} W needs bring-up \
             stage `keying` to have passed, but station.commissioned is `{stage}` \
             (docs/first-contact.md)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
