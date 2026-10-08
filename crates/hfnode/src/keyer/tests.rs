use super::*;
use keyer_core::keyer::Boot;

/// The example config with the keyer box switched on as its comments describe.
fn example_keyer() -> Config {
    // A Windows checkout has CRLF line ends.
    let text = include_str!("../../../../hfnode.example.toml")
        .replace("\r\n", "\n")
        .replace("# rig = \"keyer\"", "rig = \"keyer\"")
        .replace("# [keyer]\n", "[keyer]\n")
        // Only the [keyer] line: the [handheld] one is left commented out.
        .replace(
            "# commissioned = \"none\"           # last bring-up",
            "commissioned = \"none\"           # last bring-up",
        )
        .replace("# sidetone_hz = 600", "sidetone_hz = 600")
        .replace("# min_level_dbfs = -65", "min_level_dbfs = -65")
        .replace("max_key_seconds = 45", "max_key_seconds = 46");
    let cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.station.rig, RigKind::Keyer);
    cfg
}

#[test]
fn the_example_config_works_with_the_keyer_box() {
    let cfg = example_keyer();
    cfg.validate().unwrap();
    let k = cfg.keyer.as_ref().unwrap();
    assert_eq!(k.commissioned, Stage::None);
    assert_eq!(k.sidetone_hz, Some(600.0));
    // The figures the example's comment gives for max_key_seconds.
    let at = |wpm| {
        (longest_piece(wpm, Output::Key).unwrap() + KEY_SECONDS_SPARE)
            .as_secs_f32()
            .ceil()
    };
    assert_eq!(at(18), 46.0);
    assert_eq!(at(20), 42.0);
    let mut short = cfg.clone();
    short.station.max_key_seconds = 45;
    let e = short.validate().unwrap_err().to_string();
    assert!(e.contains("at least 46 at 18 wpm"), "{e}");
}

#[test]
fn keyer_settings_are_checked() {
    let bad = |edit: fn(&mut Config), says: &str| {
        let mut cfg = example_keyer();
        edit(&mut cfg);
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    };
    bad(|c| c.keyer = None, "no [keyer] section");
    bad(
        |c| c.station.frequency_hz = 5_357_000,
        "outside the US amateur bands",
    );
    bad(
        |c| c.station.frequency_hz = 7_300_001,
        "outside the US amateur bands",
    );
    bad(|c| c.station.serial_port = " ".into(), "serial_port");
    bad(|c| c.station.max_key_seconds = 61, "60 or less");
    bad(
        |c| c.station.key_speed_wpm = 13,
        "too slow for the keyer box",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().sidetone_hz = Some(250.0),
        "keyer.sidetone_hz",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().sidetone_hz = Some(1300.0),
        "keyer.sidetone_hz",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().min_level_dbfs = -10.0,
        "keyer.min_level_dbfs",
    );
    // Outside the IC-7300's coverage but on 2 m: fine for any radio.
    let mut cfg = example_keyer();
    cfg.station.frequency_hz = 144_050_000;
    cfg.validate().unwrap();
    // The audio pitch is the sidetone's when sidetone_hz is left out.
    cfg.keyer.as_mut().unwrap().sidetone_hz = None;
    cfg.audio.pitch_hz = 1250.0;
    assert!(cfg.validate().is_err());
}

#[test]
fn each_keying_command_needs_its_bring_up_stage() {
    use Action::*;
    let allowed = |stage| {
        [Key, Test, Run]
            .into_iter()
            .filter(|&a| check_stage(stage, a).is_ok())
            .collect::<Vec<_>>()
    };
    assert_eq!(allowed(Stage::None), []);
    assert_eq!(allowed(Stage::Listen), [Key]);
    assert_eq!(allowed(Stage::Keying), [Key, Test]);
    assert_eq!(allowed(Stage::Done), [Key, Test, Run]);
    let e = check_stage(Stage::Keying, Run).unwrap_err().to_string();
    assert!(e.contains("`run` needs bring-up stage `done`"), "{e}");
}

#[test]
fn a_box_with_looser_limits_than_the_node_is_refused() {
    let ok = Hello {
        version: keyer_core::VERSION,
        run_limit: MAX_RUN_LIMIT,
        link_timeout: MAX_LINK_TIMEOUT,
        key_down_limit: MAX_KEY_DOWN_LIMIT,
        rest: MIN_REST,
        duty_budget: MAX_DUTY_BUDGET,
        ptt_limit: MAX_PTT_LIMIT,
        uptime: Duration::from_secs(5),
        boot: Boot::Power,
        build: "1a2b3c4d".into(),
        name: keyer_core::NAME.into(),
    };
    check_hello(&ok).unwrap();
    let bad = |edit: fn(&mut Hello)| {
        let mut h = ok.clone();
        edit(&mut h);
        check_hello(&h).is_err()
    };
    assert!(bad(|h| h.version += 1));
    assert!(bad(|h| h.run_limit += Duration::from_millis(1)));
    assert!(bad(|h| h.run_limit = Duration::ZERO));
    assert!(bad(|h| h.key_down_limit += Duration::from_millis(1)));
    assert!(bad(|h| h.key_down_limit = Duration::ZERO));
    assert!(bad(|h| h.link_timeout += Duration::from_millis(1)));
    assert!(bad(|h| h.link_timeout = Duration::from_millis(999)));
    assert!(bad(|h| h.rest -= Duration::from_millis(1)));
    assert!(bad(|h| h.duty_budget += Duration::from_secs(1)));
    assert!(bad(|h| h.duty_budget = Duration::ZERO));
    // Some other device that answers HELLO.
    assert!(bad(|h| h.name = "NR7Y-CW".into()));
    // A build is checked only if one is set.
    check_build(&ok, None).unwrap();
    check_build(&ok, Some("1a2b3c4d")).unwrap();
    assert!(check_build(&ok, Some("deadbeef")).is_err());
}

#[test]
fn the_keyer_duty_window_is_no_looser_than_half_of_ten_minutes() {
    let mut cfg = example_keyer();
    let k = cfg.keyer.as_ref().unwrap();
    assert_eq!((k.max_duty_percent, k.duty_window_secs), (50, 600));
    cfg.keyer.as_mut().unwrap().max_duty_percent = 51;
    assert!(cfg.validate().is_err());
    let mut cfg = example_keyer();
    cfg.keyer.as_mut().unwrap().duty_window_secs = 601;
    assert!(cfg.validate().is_err());
    // 25% of 200 s is under one run limit.
    let mut cfg = example_keyer();
    let k = cfg.keyer.as_mut().unwrap();
    (k.max_duty_percent, k.duty_window_secs) = (25, 200);
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("allows 50 s"), "{e}");
    let k = cfg.keyer.as_mut().unwrap();
    (k.max_duty_percent, k.duty_window_secs) = (20, 300);
    cfg.validate().unwrap();
    cfg.keyer.as_mut().unwrap().firmware_build = Some("two words".into());
    assert!(cfg.validate().is_err());
    // What any build from a working tree reports: no check at all.
    cfg.keyer.as_mut().unwrap().firmware_build = Some("-".into());
    let e = cfg.validate().unwrap_err().to_string();
    assert!(e.contains("matches any firmware"), "{e}");
    cfg.keyer.as_mut().unwrap().firmware_build = Some("2f06017c".into());
    cfg.validate().unwrap();
}

#[test]
fn the_box_is_known_by_its_usb_product_name() {
    assert!(is_keyer_box("PICO2-KEYER"));
    assert!(!is_keyer_box("CP2102 USB to UART Bridge Controller"));
    assert!(!is_keyer_box("Pico"));
}

/// [`example_keyer`] on a handheld's PTT (`[keyer] output = "ptt"`), on 2 m.
fn example_handheld() -> Config {
    let mut cfg = example_keyer();
    cfg.station.frequency_hz = 144_150_000;
    cfg.station.key_speed_wpm = 20;
    cfg.station.max_key_seconds = 60;
    let k = cfg.keyer.as_mut().unwrap();
    k.output = Output::Ptt;
    k.ptt_contact_volts = Some(3.3);
    cfg
}

#[test]
fn a_handheld_on_the_ptt_is_checked() {
    example_handheld().validate().unwrap();
    let bad = |edit: fn(&mut Config), says: &str| {
        let mut cfg = example_handheld();
        edit(&mut cfg);
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    };
    // MCW only where 97.305(c) allows it: not the bottom of 2 m, not HF.
    bad(|c| c.station.frequency_hz = 144_050_000, "MCW is allowed");
    bad(|c| c.station.frequency_hz = 7_030_000, "MCW is allowed");
    bad(
        |c| c.keyer.as_mut().unwrap().ptt_contact_volts = Some(1.0),
        "ptt_contact_volts",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().ptt_contact_volts = Some(30.0),
        "ptt_contact_volts",
    );
    // 30 zeros at 13 wpm are 60.6 s: with the lead and tail, not under the box's
    // 60 s PTT limit.
    bad(
        |c| c.station.key_speed_wpm = 13,
        "with the PTT's lead and tail",
    );
    // The longest piece with its lead and tail, the spare, and the receive
    // noise's return: 39.4 + 0.7 + 2 + 1.5 s at 20 wpm.
    bad(|c| c.station.max_key_seconds = 43, "max_key_seconds");
    let mut ok = example_handheld();
    ok.station.max_key_seconds = 44;
    ok.validate().unwrap();
    ok.station.frequency_hz = 446_000_000;
    ok.validate().unwrap();
}

#[test]
fn nothing_keys_a_handheld_until_its_ptt_contact_is_on_record() {
    let mut cfg = example_handheld();
    let k = cfg.keyer.as_mut().unwrap();
    check_ptt_cable(k).unwrap();
    k.ptt_contact_volts = None;
    let e = check_ptt_cable(k).unwrap_err().to_string();
    assert!(e.contains("not recorded"), "{e}");
    // Not needed to validate: the bring-up records it.
    cfg.validate().unwrap();
    // Nor for the key output.
    let k = cfg.keyer.as_mut().unwrap();
    k.output = Output::Key;
    check_ptt_cable(k).unwrap();
}

#[test]
fn the_output_is_read_from_the_config() {
    let text = |extra: &str| format!("[keyer]\n{extra}");
    let k: crate::config::Keyer = toml::from_str::<toml::Table>(&text("output = \"ptt\"")).unwrap()
        ["keyer"]
        .clone()
        .try_into()
        .unwrap();
    assert_eq!(k.output, Output::Ptt);
    let k: crate::config::Keyer = toml::from_str::<toml::Table>(&text("")).unwrap()["keyer"]
        .clone()
        .try_into()
        .unwrap();
    assert_eq!(k.output, Output::Key);
    assert!(
        toml::from_str::<toml::Table>(&text("output = \"vox\"")).unwrap()["keyer"]
            .clone()
            .try_into::<crate::config::Keyer>()
            .is_err()
    );
}
